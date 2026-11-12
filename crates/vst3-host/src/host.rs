//! The COM objects WAVE implements *for* a VST3 plugin.
//!
//! | object | given to | why it cannot be null |
//! | --- | --- | --- |
//! | [`HostContext`] | `initialize` on both halves, `setComponentHandler` | plugins ask for the host's name, and the editor reports knob moves through `IComponentHandler` |
//! | [`ParameterChanges`] | `ProcessData::{input,output}ParameterChanges` | how parameter moves reach the processor; nominally optional, dereferenced anyway |
//! | [`EmptyEventList`] | `ProcessData::inputEvents` | effects with an event bus dereference it too |
//! | [`PlugFrame`] | `IPlugView::setFrame` | how an editor asks for a size |
//! | [`MemoryStream`] | `getState` / `setState` | there is no other way to move a state blob |
//!
//! The `vst3` crate has no macro for this: a type lists its interfaces in
//! [`Class::Interfaces`], implements the `…Trait`s (all `&self`), and is handed
//! out through [`ComWrapper`], which owns the refcount.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use vst3::Steinberg::Vst::{
    Event, IComponentHandler, IComponentHandlerTrait, IEventList, IEventListTrait,
    IHostApplication, IHostApplicationTrait, IParamValueQueue, IParamValueQueueTrait,
    IParameterChanges, IParameterChangesTrait, ParamID, ParamValue, String128,
};
use vst3::Steinberg::{
    int32, int64, kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, tresult, FUnknown,
    IBStream, IBStreamTrait, IPlugFrame, IPlugFrameTrait, IPlugView, ViewRect, TUID,
};
use vst3::{Class, ComWrapper};

use crate::module::write_utf16;

/// Host identity handed to every plugin.
pub(crate) const HOST_NAME: &str = "WAVE";

/// Largest state blob accepted from, or handed to, a plugin. Guards against a
/// plugin that streams without end.
pub const MAX_STATE_BYTES: usize = 64 * 1024 * 1024;

/// Parameter changes delivered to the processor per block. More are left in
/// the mailbox for the next block, never dropped.
pub(crate) const MAX_CHANGES_PER_BLOCK: usize = 64;

/// Points one queue holds. Input queues use one (WAVE sends no ramps); output
/// queues take what the plugin writes, up to this.
const POINTS_PER_QUEUE: usize = 16;

/// Capacity of the overflow ring for parameter ids the controller did not list
/// at instantiation (rare: ids a plugin adds later).
pub(crate) const UNLISTED_RING: usize = 256;

/// Most parameters a plugin may list; past it the rest go through the ring.
const MAX_LISTED_PARAMS: usize = 1 << 16;

// ---------------------------------------------------------------------------
// The parameter mailbox
// ---------------------------------------------------------------------------

/// A coalescing, lock-free parameter mailbox: one slot per parameter, so only
/// the **latest** value of each is delivered and nothing is lost however long
/// the consumer stays away (a ring would fill up while audio is stopped and
/// drop the newest knob positions, leaving processor and editor disagreeing).
///
/// The id list is fixed at creation (the controller's parameters at
/// instantiation); the id -> slot map is immutable afterwards, so lookups on
/// either side are plain reads. Posting stores the value, then sets a dirty
/// bit (and a summary bit per 64 dirty words); draining swaps the bits out,
/// so its cost is proportional to what changed, not to the parameter count.
///
/// Two of these exist per instance: host -> processor (posted by
/// [`Vst3Handle::set_param`](crate::Vst3Handle::set_param) and the editor's
/// `performEdit`, drained by the audio thread) and processor -> host (posted by
/// the audio thread from `outputParameterChanges`, drained on the UI thread
/// into the controller).
pub(crate) struct ParamMailbox {
    slot_of: std::collections::HashMap<ParamID, usize>,
    ids: Vec<ParamID>,
    values: Vec<std::sync::atomic::AtomicU64>,
    dirty: Vec<AtomicU64>,
    summary: Vec<AtomicU64>,
    /// Ids not in `ids`. Non-RT producers only (it is behind a mutex).
    unlisted: Mutex<Option<rtrb::Producer<(ParamID, ParamValue)>>>,
}

impl ParamMailbox {
    /// A mailbox for `ids`, plus the consumer half of its overflow ring (owned
    /// by whoever drains). Allocates; call it off the audio thread.
    pub(crate) fn new(ids: &[ParamID]) -> (Self, rtrb::Consumer<(ParamID, ParamValue)>) {
        let mut unique: Vec<ParamID> = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        unique.truncate(MAX_LISTED_PARAMS);
        let words = unique.len().div_ceil(64).max(1);
        let (producer, consumer) = rtrb::RingBuffer::new(UNLISTED_RING);
        let mailbox = Self {
            slot_of: unique
                .iter()
                .enumerate()
                .map(|(slot, id)| (*id, slot))
                .collect(),
            values: unique
                .iter()
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
            ids: unique,
            dirty: (0..words).map(|_| AtomicU64::new(0)).collect(),
            summary: (0..words.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
            unlisted: Mutex::new(Some(producer)),
        };
        (mailbox, consumer)
    }

    /// Post a value for a listed id. Lock-free and allocation-free (RT-safe);
    /// `false` for an id that was not listed.
    pub(crate) fn post_listed(&self, id: ParamID, value: ParamValue) -> bool {
        let Some(&slot) = self.slot_of.get(&id) else {
            return false;
        };
        self.values[slot].store(value.to_bits(), Ordering::Release);
        let word = slot / 64;
        self.dirty[word].fetch_or(1 << (slot % 64), Ordering::Release);
        self.summary[word / 64].fetch_or(1 << (word % 64), Ordering::Release);
        true
    }

    /// Post from a non-RT thread: listed ids coalesce, others go through the
    /// overflow ring. `false` only when that ring is full.
    pub(crate) fn post(&self, id: ParamID, value: ParamValue) -> bool {
        if self.post_listed(id, value) {
            return true;
        }
        self.unlisted
            .lock()
            .as_mut()
            .is_some_and(|ring| ring.push((id, value)).is_ok())
    }

    /// Hand at most `limit` pending `(id, value)` pairs to `sink`, latest
    /// value per id; whatever does not fit stays pending. Single consumer.
    /// RT-safe: atomics only.
    pub(crate) fn drain(&self, limit: usize, mut sink: impl FnMut(ParamID, ParamValue)) -> usize {
        let mut delivered = 0;
        for (summary_index, summary) in self.summary.iter().enumerate() {
            if delivered == limit {
                break;
            }
            let mut words = summary.swap(0, Ordering::AcqRel);
            while words != 0 {
                let word_index = summary_index * 64 + words.trailing_zeros() as usize;
                words &= words - 1;
                let mut bits = self.dirty[word_index].swap(0, Ordering::AcqRel);
                while bits != 0 && delivered < limit {
                    let slot = word_index * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let value = f64::from_bits(self.values[slot].load(Ordering::Acquire));
                    sink(self.ids[slot], value);
                    delivered += 1;
                }
                if bits != 0 {
                    // Out of room: put the rest back for the next drain.
                    self.dirty[word_index].fetch_or(bits, Ordering::AcqRel);
                    words |= 1 << (word_index % 64);
                }
                if delivered == limit {
                    if words != 0 {
                        summary.fetch_or(words, Ordering::AcqRel);
                    }
                    break;
                }
            }
        }
        delivered
    }

    /// The listed ids, sorted.
    #[cfg(test)]
    pub(crate) fn ids(&self) -> &[ParamID] {
        &self.ids
    }
}

/// What the plugin has told the host since the last look, as atomics: these
/// callbacks may arrive from any plugin thread and must not block.
#[derive(Debug, Default)]
pub(crate) struct HostFlags {
    /// OR of every `restartComponent` flag. Recorded, not acted on.
    pub(crate) restart: AtomicI32,
    /// A `performEdit` happened: the saved state is stale.
    pub(crate) dirty: AtomicBool,
}

// ---------------------------------------------------------------------------
// IHostApplication + IComponentHandler
// ---------------------------------------------------------------------------

/// The host context both plugin halves are initialised with, and the component
/// handler the controller reports edits through.
///
/// `createInstance` (for `IMessage`/`IAttributeList`) answers
/// `kNotImplemented`: legal and tolerated by every plugin, but a
/// dual-component plugin that relies on `IConnectionPoint` messaging will have
/// an editor that hears less from its processor.
pub(crate) struct HostContext {
    /// Host -> processor mailbox. Set once the controller has listed its
    /// parameters; an edit reported before that (during `initialize`) has no
    /// processor to reach yet and is dropped.
    inbox: OnceLock<Arc<ParamMailbox>>,
    flags: Arc<HostFlags>,
}

impl HostContext {
    pub(crate) fn new(flags: Arc<HostFlags>) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            inbox: OnceLock::new(),
            flags,
        })
    }

    pub(crate) fn connect_inbox(&self, inbox: Arc<ParamMailbox>) {
        let _ = self.inbox.set(inbox);
    }
}

impl Class for HostContext {
    type Interfaces = (IHostApplication, IComponentHandler);
}

impl IHostApplicationTrait for HostContext {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        write_utf16(HOST_NAME, &mut *name);
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        _cid: *mut TUID,
        _iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        if !obj.is_null() {
            *obj = std::ptr::null_mut();
        }
        kNotImplemented
    }
}

impl IComponentHandlerTrait for HostContext {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    /// A knob moved in the editor. The controller already knows; the processor
    /// learns through the mailbox, which is the whole point of this callback.
    unsafe fn performEdit(&self, id: ParamID, value_normalized: ParamValue) -> tresult {
        self.flags.dirty.store(true, Ordering::Release);
        if !value_normalized.is_finite() {
            return kInvalidArgument;
        }
        match self.inbox.get() {
            Some(inbox) if !inbox.post(id, value_normalized.clamp(0.0, 1.0)) => kResultFalse,
            _ => kResultOk,
        }
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.flags.restart.fetch_or(flags, Ordering::AcqRel);
        self.flags.dirty.store(true, Ordering::Release);
        kResultOk
    }
}

/// The `FUnknown` a plugin's `initialize` is given.
pub(crate) fn host_context_ptr(context: &ComWrapper<HostContext>) -> *mut FUnknown {
    context
        .as_com_ref::<IHostApplication>()
        .map_or(std::ptr::null_mut(), |host| {
            host.as_ptr().cast::<FUnknown>()
        })
}

// ---------------------------------------------------------------------------
// IPlugFrame
// ---------------------------------------------------------------------------

/// What an editor asked its window to become, latched: `resizeView` may come
/// from a plugin thread, and touching the window is UI-thread work done at the
/// next tick.
#[derive(Debug, Default)]
pub(crate) struct GuiRequests {
    width: AtomicI32,
    height: AtomicI32,
    pending: AtomicBool,
}

impl GuiRequests {
    pub(crate) fn take_resize(&self) -> Option<(i32, i32)> {
        self.pending.swap(false, Ordering::AcqRel).then(|| {
            (
                self.width.load(Ordering::Acquire),
                self.height.load(Ordering::Acquire),
            )
        })
    }

    /// Forget a stale request so it cannot reach the next editor session.
    pub(crate) fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

/// The frame an editor is given. `kResultOk` means "acknowledged", not "done".
pub(crate) struct PlugFrame {
    requests: Arc<GuiRequests>,
}

impl PlugFrame {
    pub(crate) fn new(requests: Arc<GuiRequests>) -> ComWrapper<Self> {
        ComWrapper::new(Self { requests })
    }
}

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        let Some(rect) = new_size.as_ref() else {
            return kInvalidArgument;
        };
        self.requests
            .width
            .store(rect.right - rect.left, Ordering::Release);
        self.requests
            .height
            .store(rect.bottom - rect.top, Ordering::Release);
        self.requests.pending.store(true, Ordering::Release);
        kResultOk
    }
}

// ---------------------------------------------------------------------------
// IParameterChanges
// ---------------------------------------------------------------------------

/// One parameter's points within a block.
pub(crate) struct ValueQueue {
    id: Cell<ParamID>,
    count: Cell<usize>,
    points: [Cell<(int32, ParamValue)>; POINTS_PER_QUEUE],
}

impl ValueQueue {
    fn new() -> Self {
        Self {
            id: Cell::new(0),
            count: Cell::new(0),
            points: std::array::from_fn(|_| Cell::new((0, 0.0))),
        }
    }

    fn last(&self) -> Option<ParamValue> {
        let count = self.count.get();
        (count > 0).then(|| self.points[count - 1].get().1)
    }
}

impl Class for ValueQueue {
    type Interfaces = (IParamValueQueue,);
}

// SAFETY: `Cell` is not `Sync`, but these objects are owned by one `Vst3Effect`
// and touched only by the thread that owns it -- by us when filling them, by
// the plugin when reading them, both inside the same `process` call. The plugin
// must not retain block-local interfaces past `process` (VST3 rule).
unsafe impl Send for ValueQueue {}
unsafe impl Sync for ValueQueue {}

impl IParamValueQueueTrait for ValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.count.get() as int32
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index < 0 || index as usize >= self.count.get() || offset.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        let (time, point) = self.points[index as usize].get();
        *offset = time;
        *value = point;
        kResultOk
    }

    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        let count = self.count.get();
        if count >= POINTS_PER_QUEUE || offset < 0 || !value.is_finite() || index.is_null() {
            return kInvalidArgument;
        }
        self.points[count].set((offset, value));
        self.count.set(count + 1);
        *index = count as int32;
        kResultOk
    }
}

/// A block's parameter changes, preallocated: nothing here allocates after
/// [`new`](Self::new).
pub(crate) struct ParameterChanges {
    queues: Vec<ComWrapper<ValueQueue>>,
    count: Cell<usize>,
}

// SAFETY: as for `ValueQueue`.
unsafe impl Send for ParameterChanges {}
unsafe impl Sync for ParameterChanges {}

impl ParameterChanges {
    pub(crate) fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            queues: (0..MAX_CHANGES_PER_BLOCK)
                .map(|_| ComWrapper::new(ValueQueue::new()))
                .collect(),
            count: Cell::new(0),
        })
    }

    /// Empty the list, keeping its storage. RT-safe.
    pub(crate) fn clear(&self) {
        for queue in &self.queues[..self.count.get()] {
            queue.count.set(0);
        }
        self.count.set(0);
    }

    /// Number of parameters with changes this block.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.count.get()
    }

    /// Refill from the mailbox and its overflow ring. RT-safe: atomics and
    /// lock-free pops into preallocated queues. Whatever does not fit stays
    /// pending for the next block.
    pub(crate) fn fill(
        &self,
        mailbox: &ParamMailbox,
        unlisted: &mut rtrb::Consumer<(ParamID, ParamValue)>,
    ) {
        self.clear();
        mailbox.drain(self.queues.len(), |id, value| {
            let used = self.count.get();
            let queue = &self.queues[used];
            queue.id.set(id);
            queue.points[0].set((0, value));
            queue.count.set(1);
            self.count.set(used + 1);
        });
        while let Ok(&(id, value)) = unlisted.peek() {
            let used = self.count.get();
            let queue = match self.queues[..used]
                .iter()
                .find(|queue| queue.id.get() == id)
            {
                Some(queue) => queue,
                None if used < self.queues.len() => {
                    let queue = &self.queues[used];
                    queue.id.set(id);
                    self.count.set(used + 1);
                    queue
                }
                None => break,
            };
            queue.points[0].set((0, value));
            queue.count.set(1);
            let _ = unlisted.pop();
        }
    }

    /// Hand every parameter's final value to `sink` (processor -> host feedback).
    pub(crate) fn for_each_last(&self, mut sink: impl FnMut(ParamID, ParamValue)) {
        for queue in &self.queues[..self.count.get()] {
            if let Some(value) = queue.last() {
                sink(queue.id.get(), value);
            }
        }
    }
}

impl Class for ParameterChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParameterChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.count.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index < 0 || index as usize >= self.count.get() {
            return std::ptr::null_mut();
        }
        self.queues[index as usize]
            .as_com_ref::<IParamValueQueue>()
            .map_or(std::ptr::null_mut(), |queue| queue.as_ptr())
    }

    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut int32,
    ) -> *mut IParamValueQueue {
        let (Some(&id), false) = (id.as_ref(), index.is_null()) else {
            return std::ptr::null_mut();
        };
        let used = self.count.get();
        let position = match self.queues[..used]
            .iter()
            .position(|queue| queue.id.get() == id)
        {
            Some(position) => position,
            None if used < self.queues.len() => {
                self.queues[used].id.set(id);
                self.queues[used].count.set(0);
                self.count.set(used + 1);
                used
            }
            None => return std::ptr::null_mut(),
        };
        *index = position as int32;
        self.getParameterData(position as int32)
    }
}

// ---------------------------------------------------------------------------
// IEventList
// ---------------------------------------------------------------------------

/// An input event list that is always empty: WAVE hosts effects and sends
/// them no events, but a plugin with an event bus may not check for null.
pub(crate) struct EmptyEventList;

impl EmptyEventList {
    pub(crate) fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self)
    }
}

impl Class for EmptyEventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EmptyEventList {
    unsafe fn getEventCount(&self) -> int32 {
        0
    }

    unsafe fn getEvent(&self, _index: int32, _event: *mut Event) -> tresult {
        kInvalidArgument
    }

    /// The host's input list must not grow under the audio thread.
    unsafe fn addEvent(&self, _event: *mut Event) -> tresult {
        kNotImplemented
    }
}

// ---------------------------------------------------------------------------
// IBStream
// ---------------------------------------------------------------------------

const SEEK_SET: int32 = 0;
const SEEK_CUR: int32 = 1;
const SEEK_END: int32 = 2;

/// An in-memory `IBStream`: VST3 has no "give me your state as bytes"; the
/// plugin writes into a stream the host provides. UI-thread only.
pub(crate) struct MemoryStream {
    buffer: RefCell<Vec<u8>>,
    cursor: Cell<usize>,
}

impl MemoryStream {
    pub(crate) fn empty() -> ComWrapper<Self> {
        Self::reading(&[])
    }

    pub(crate) fn reading(bytes: &[u8]) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            buffer: RefCell::new(bytes.to_vec()),
            cursor: Cell::new(0),
        })
    }

    pub(crate) fn contents(&self) -> Vec<u8> {
        self.buffer.borrow().clone()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buffer.borrow().is_empty()
    }

    /// Back to the start, for a second reader of the same blob.
    pub(crate) fn rewind(&self) {
        self.cursor.set(0);
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_bytes_read: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        let source = self.buffer.borrow();
        let start = self.cursor.get().min(source.len());
        let available = (source.len() - start).min(num_bytes as usize);
        if available > 0 {
            std::ptr::copy_nonoverlapping(
                source.as_ptr().add(start),
                buffer.cast::<u8>(),
                available,
            );
        }
        self.cursor.set(start + available);
        if !num_bytes_read.is_null() {
            *num_bytes_read = available as int32;
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_bytes_written: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        let wanted = num_bytes as usize;
        let start = self.cursor.get();
        if start.saturating_add(wanted) > MAX_STATE_BYTES {
            if !num_bytes_written.is_null() {
                *num_bytes_written = 0;
            }
            return kInvalidArgument;
        }
        let mut sink = self.buffer.borrow_mut();
        if sink.len() < start + wanted {
            sink.resize(start + wanted, 0);
        }
        std::ptr::copy_nonoverlapping(buffer.cast::<u8>(), sink.as_mut_ptr().add(start), wanted);
        self.cursor.set(start + wanted);
        if !num_bytes_written.is_null() {
            *num_bytes_written = num_bytes;
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let len = self.buffer.borrow().len() as i64;
        let base = match mode {
            SEEK_SET => 0,
            SEEK_CUR => self.cursor.get() as i64,
            SEEK_END => len,
            _ => return kInvalidArgument,
        };
        let Some(target) = base.checked_add(pos).filter(|target| *target >= 0) else {
            return kInvalidArgument;
        };
        // Past the end is legal; the gap fills with zeroes on the next write.
        self.cursor.set(target as usize);
        if !result.is_null() {
            *result = target;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        *pos = self.cursor.get() as i64;
        kResultOk
    }
}

/// Counts parameter changes the audio thread could not hand back to the host.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) feedback_dropped: AtomicU64,
    pub(crate) process_errors: AtomicU64,
    pub(crate) ok_calls: AtomicU64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::ComPtr;
    use vst3::ComRef;

    type Unlisted = rtrb::Consumer<(ParamID, ParamValue)>;

    fn mailbox(ids: &[ParamID]) -> (Arc<ParamMailbox>, Unlisted) {
        let (mailbox, unlisted) = ParamMailbox::new(ids);
        (Arc::new(mailbox), unlisted)
    }

    fn drained(mailbox: &ParamMailbox, limit: usize) -> Vec<(ParamID, ParamValue)> {
        let mut out = Vec::new();
        mailbox.drain(limit, |id, value| out.push((id, value)));
        out
    }

    fn context() -> (
        ComWrapper<HostContext>,
        Arc<ParamMailbox>,
        Unlisted,
        Arc<HostFlags>,
    ) {
        let (inbox, unlisted) = mailbox(&[7]);
        let flags = Arc::new(HostFlags::default());
        let context = HostContext::new(Arc::clone(&flags));
        context.connect_inbox(Arc::clone(&inbox));
        (context, inbox, unlisted, flags)
    }

    #[test]
    fn the_host_reports_its_name_and_refuses_a_null_buffer() {
        let (context, ..) = context();
        let host: ComPtr<IHostApplication> = context.to_com_ptr().unwrap();
        let mut name: String128 = [0xffff; 128];
        // SAFETY: `name` is a live String128; null is the point of the second call.
        unsafe {
            assert_eq!(host.getName(&mut name), kResultOk);
            assert_eq!(host.getName(std::ptr::null_mut()), kInvalidArgument);
        }
        assert_eq!(crate::module::utf16_to_string(&name), HOST_NAME);
    }

    #[test]
    fn an_editor_edit_reaches_the_processor_mailbox_and_marks_the_state_dirty() {
        let (context, inbox, mut unlisted, flags) = context();
        let handler: ComPtr<IComponentHandler> = context.to_com_ptr().unwrap();
        // SAFETY: plain scalars.
        unsafe {
            assert_eq!(handler.beginEdit(7), kResultOk);
            assert_eq!(handler.performEdit(7, 0.25), kResultOk);
            assert_eq!(
                handler.performEdit(7, 1.5),
                kResultOk,
                "clamped, not refused"
            );
            assert_eq!(handler.endEdit(7), kResultOk);
            assert_eq!(
                handler.performEdit(99, 0.5),
                kResultOk,
                "an unlisted id still arrives"
            );
            assert_eq!(handler.performEdit(7, f64::NAN), kInvalidArgument);
            assert_eq!(handler.restartComponent(2), kResultOk);
            assert_eq!(handler.restartComponent(8), kResultOk);
        }
        assert_eq!(
            drained(&inbox, 64),
            vec![(7, 1.0)],
            "moves of one knob coalesce to the latest"
        );
        assert_eq!(unlisted.pop(), Ok((99, 0.5)));
        assert!(flags.dirty.load(Ordering::Acquire));
        assert_eq!(flags.restart.load(Ordering::Acquire), 2 | 8);
    }

    #[test]
    fn edits_before_the_mailbox_exists_are_dropped_not_crashed_on() {
        let context = HostContext::new(Arc::new(HostFlags::default()));
        let handler: ComPtr<IComponentHandler> = context.to_com_ptr().unwrap();
        // SAFETY: plain scalars.
        assert_eq!(unsafe { handler.performEdit(1, 0.5) }, kResultOk);
    }

    #[test]
    fn the_mailbox_never_loses_the_latest_value_however_long_nobody_drains() {
        let ids: Vec<ParamID> = (0..1000).map(|n| n * 7 + (1 << 20)).collect();
        let (inbox, _unlisted) = mailbox(&ids);
        assert_eq!(inbox.ids().len(), 1000);
        // A long knob drag while audio is stopped: thousands of posts.
        for step in 0..10_000 {
            assert!(inbox.post(ids[3], f64::from(step) / 10_000.0));
        }
        assert!(inbox.post(ids[999], 0.5));
        assert!(inbox.post_listed(ids[0], 0.25));
        let mut got = drained(&inbox, 64);
        got.sort_by_key(|(id, _)| *id);
        assert_eq!(got, vec![(ids[0], 0.25), (ids[3], 0.9999), (ids[999], 0.5)]);
        assert!(drained(&inbox, 64).is_empty());
        assert!(
            !inbox.post_listed(12345, 0.5),
            "the RT path never takes unlisted ids"
        );
    }

    #[test]
    fn a_bounded_drain_leaves_the_rest_pending_for_the_next_one() {
        let ids: Vec<ParamID> = (0..300).collect();
        let (inbox, _unlisted) = mailbox(&ids);
        for id in &ids {
            assert!(inbox.post(*id, 0.5));
        }
        assert_eq!(drained(&inbox, 128).len(), 128);
        assert_eq!(drained(&inbox, 128).len(), 128);
        assert_eq!(drained(&inbox, 128).len(), 44);
        assert!(drained(&inbox, 128).is_empty());
    }

    #[test]
    fn the_overflow_ring_reports_full_rather_than_blocking() {
        let (inbox, _unlisted) = mailbox(&[]);
        for n in 0..UNLISTED_RING as u32 {
            assert!(inbox.post(n, 0.5));
        }
        assert!(!inbox.post(9999, 0.5));
    }

    #[test]
    fn a_plug_frame_records_the_size_asked_for_and_refuses_null() {
        let requests = Arc::new(GuiRequests::default());
        let frame: ComPtr<IPlugFrame> = PlugFrame::new(Arc::clone(&requests)).to_com_ptr().unwrap();
        let mut rect = ViewRect {
            left: 10,
            top: 20,
            right: 810,
            bottom: 620,
        };
        // SAFETY: `rect` is live for the call; the view is unused.
        unsafe {
            assert_eq!(frame.resizeView(std::ptr::null_mut(), &mut rect), kResultOk);
            assert_eq!(requests.take_resize(), Some((800, 600)));
            assert_eq!(requests.take_resize(), None, "reading clears it");
            let _ = frame.resizeView(std::ptr::null_mut(), &mut rect);
            requests.clear();
            assert_eq!(
                requests.take_resize(),
                None,
                "a stale request must not survive"
            );
            assert_eq!(
                frame.resizeView(std::ptr::null_mut(), std::ptr::null_mut()),
                kInvalidArgument
            );
        }
    }

    #[test]
    fn parameter_changes_carry_the_latest_values_and_leave_overflow_pending() {
        let ids: Vec<ParamID> = (0..200).collect();
        let (inbox, mut unlisted) = mailbox(&ids);
        let changes = ParameterChanges::new();
        assert!(inbox.post(17, 0.2));
        assert!(inbox.post(17, 0.7));
        assert!(inbox.post(500, 0.1), "unlisted");
        changes.fill(&inbox, &mut unlisted);
        assert_eq!(changes.len(), 2, "two moves of one parameter are one queue");
        // SAFETY: indices are in range and the out-params are live locals.
        unsafe {
            let queue = ComRef::<IParamValueQueue>::from_raw(changes.getParameterData(0)).unwrap();
            assert_eq!(queue.getParameterId(), 17);
            assert_eq!(queue.getPointCount(), 1);
            let (mut offset, mut value) = (-1, 0.0);
            assert_eq!(queue.getPoint(0, &mut offset, &mut value), kResultOk);
            assert_eq!((offset, value), (0, 0.7), "the latest value wins");
            let queue = ComRef::<IParamValueQueue>::from_raw(changes.getParameterData(1)).unwrap();
            assert_eq!(queue.getParameterId(), 500);
            assert!(changes.getParameterData(2).is_null());
        }

        for id in 0..(MAX_CHANGES_PER_BLOCK as u32 + 5) {
            assert!(inbox.post(id, 0.5));
        }
        changes.fill(&inbox, &mut unlisted);
        assert_eq!(changes.len(), MAX_CHANGES_PER_BLOCK);
        changes.fill(&inbox, &mut unlisted);
        assert_eq!(
            changes.len(),
            5,
            "the overflow arrives a block later, not never"
        );
        changes.fill(&inbox, &mut unlisted);
        assert_eq!(changes.len(), 0);
    }

    #[test]
    fn output_changes_written_by_the_plugin_come_back_as_final_values() {
        let changes = ParameterChanges::new();
        let list: ComPtr<IParameterChanges> = changes.to_com_ptr().unwrap();
        // SAFETY: live out-params; the queue pointer is used within this scope.
        unsafe {
            let mut index = -1;
            let queue = ComRef::<IParamValueQueue>::from_raw(list.addParameterData(&9, &mut index))
                .unwrap();
            assert_eq!(index, 0);
            let mut point = -1;
            assert_eq!(queue.addPoint(0, 0.1, &mut point), kResultOk);
            assert_eq!(queue.addPoint(32, 0.9, &mut point), kResultOk);
            assert_eq!(queue.addPoint(-1, 0.9, &mut point), kInvalidArgument);
            let mut again = -1;
            assert!(!list.addParameterData(&9, &mut again).is_null());
            assert_eq!(again, 0, "the same id reuses its queue");
        }
        let mut seen = Vec::new();
        changes.for_each_last(|id, value| seen.push((id, value)));
        assert_eq!(seen, vec![(9, 0.9)]);
        changes.clear();
        assert_eq!(changes.len(), 0);
    }

    #[test]
    fn the_empty_event_list_is_empty_and_cannot_be_grown() {
        let list: ComPtr<IEventList> = EmptyEventList::new().to_com_ptr().unwrap();
        // SAFETY: a zeroed Event is a valid value; null out-pointers are refused.
        unsafe {
            assert_eq!(list.getEventCount(), 0);
            let mut event: Event = std::mem::zeroed();
            assert_eq!(list.getEvent(0, &mut event), kInvalidArgument);
            assert_eq!(list.addEvent(&mut event), kNotImplemented);
        }
    }

    #[test]
    fn a_memory_stream_round_trips_a_blob_and_seeks_from_all_origins() {
        let blob: Vec<u8> = (0..300_u32).map(|i| (i % 251) as u8).collect();
        let sink = MemoryStream::empty();
        let stream: ComPtr<IBStream> = sink.to_com_ptr().unwrap();
        let mut written = 0;
        // SAFETY: buffers and out-params are live locals.
        unsafe {
            assert_eq!(
                stream.write(
                    blob.as_ptr() as *mut c_void,
                    blob.len() as int32,
                    &mut written
                ),
                kResultOk
            );
        }
        assert_eq!(written as usize, blob.len());
        assert_eq!(sink.contents(), blob);

        let source = MemoryStream::reading(&blob);
        let stream: ComPtr<IBStream> = source.to_com_ptr().unwrap();
        let mut out = vec![0_u8; blob.len() + 16];
        let mut read = 0;
        let mut at: int64 = -1;
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                stream.read(out.as_mut_ptr().cast(), out.len() as int32, &mut read),
                kResultOk
            );
            assert_eq!(
                read as usize,
                blob.len(),
                "a short read at the end is normal"
            );
            assert_eq!(stream.seek(4, SEEK_SET, &mut at), kResultOk);
            assert_eq!(at, 4);
            assert_eq!(stream.seek(2, SEEK_CUR, &mut at), kResultOk);
            assert_eq!(at, 6);
            assert_eq!(stream.seek(-300, SEEK_END, &mut at), kResultOk);
            assert_eq!(at, 0);
            assert_eq!(stream.seek(-1, SEEK_SET, &mut at), kInvalidArgument);
            assert_eq!(stream.seek(0, 99, &mut at), kInvalidArgument);
            assert_eq!(stream.tell(std::ptr::null_mut()), kInvalidArgument);
        }
        assert_eq!(&out[..blob.len()], &blob[..]);
        source.rewind();
        let mut first = [0_u8; 1];
        // SAFETY: as above.
        unsafe { stream.read(first.as_mut_ptr().cast(), 1, &mut read) };
        assert_eq!(first[0], blob[0]);
    }

    #[test]
    fn a_stream_write_past_the_size_limit_is_refused_rather_than_allocated() {
        let sink = MemoryStream::empty();
        let stream: ComPtr<IBStream> = sink.to_com_ptr().unwrap();
        let chunk = [0_u8; 16];
        let (mut at, mut written) = (0, -1);
        // SAFETY: live locals.
        unsafe {
            assert_eq!(
                stream.seek(MAX_STATE_BYTES as i64, SEEK_SET, &mut at),
                kResultOk
            );
            assert_eq!(
                stream.write(chunk.as_ptr() as *mut c_void, 16, &mut written),
                kInvalidArgument
            );
        }
        assert_eq!(written, 0);
        assert!(sink.is_empty(), "nothing may have been allocated");
    }
}
