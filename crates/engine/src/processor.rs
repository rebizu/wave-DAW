//! The one interface every effect in an insert chain implements — built-in DSP and hosted VST3 alike.

/// Live readings per effect for its visualisation.
pub const METERS: usize = 32;

/// What the audio thread knows about the moment being processed.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCtx {
    pub sample_rate: f32,
    pub bpm: f64,
    /// Playhead at the first sample of this block, in beats.
    pub beat: f64,
    pub playing: bool,
}

/// A stereo, in-place audio effect.
///
/// Real-time contract for [`Processor::process`]: no allocation, no locks that
/// a non-audio thread can hold for long, no I/O, no panics. Everything that
/// needs memory happens in [`Processor::prepare`], which is called **off** the
/// audio thread before the processor is handed to it.
pub trait Processor: Send {
    /// Allocate for `sample_rate` and blocks of at most `max_block` frames.
    fn prepare(&mut self, sample_rate: f32, max_block: usize);

    /// Process `l.len()` frames in place. `l.len() == r.len() <= max_block`.
    fn process(&mut self, l: &mut [f32], r: &mut [f32], ctx: &ProcessCtx);

    /// Apply a parameter set (built-in effects). Called on the audio thread,
    /// so it must not allocate either. Unknown ids are ignored.
    fn set_params(&mut self, _params: &ParamBlock) {}

    /// A hosted plugin's parameter, `normalized` 0..1 (`AutoTarget::Plugin`
    /// automation; the plugin's own parameter id, as `vst3_params` reports
    /// it). No-op for built-in effects. Unknown ids are ignored. Audio thread:
    /// no allocation, no lock.
    fn set_plugin_param(&mut self, _param_id: u32, _normalized: f32) {}

    /// Forget tails and envelopes (transport jump, bypass toggled on).
    fn reset(&mut self) {}

    /// How many samples of output this processor can still produce, and how long its
    /// state needs to settle, after its input became exact digital silence (the time to
    /// fall below about -140 dB / to forget envelopes). `Some(0)` = memoryless. `None` =
    /// unknown or self-generating: the chain is never skipped. Called on the audio
    /// thread: no allocation.
    fn tail_samples(&self) -> Option<usize> {
        None
    }

    /// Called once when the chain has been silent for longer than every tail and is
    /// about to be skipped: bring the state to what the tail would have left (zeros,
    /// settled envelopes and smoothers). Defaults to `reset`.
    fn park(&mut self) {
        self.reset();
    }

    /// Current gain reduction in dB (≤ 0) for meters; 0 when not a dynamics processor.
    fn gain_reduction_db(&self) -> f32 {
        0.0
    }

    /// Up to 32 live readings for the effect's visualisation (pitch, envelope, …);
    /// meaning is per effect type, documented in docs/architecture.md.
    fn meters(&self, _out: &mut [f32; METERS]) {}

    /// Mono tap of the wet signal only, for effects whose interesting output is
    /// not the mix (reverb). `None` → analysers use the processor output.
    fn wet_tap(&self) -> Option<&[f32]> {
        None
    }
}

/// Parameters of a built-in effect, already resolved to numbers on the
/// non-audio side. Fixed size so it travels through a ring buffer by value.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ParamBlock {
    /// Built-in effect parameters, in the order of the type's parameter list (see `fx::params`).
    pub values: [f32; 12],
    /// EQ bands (unused slots have `on = false`).
    pub bands: [BandParams; 24],
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BandParams {
    pub on: bool,
    /// 0 lowcut, 1 lowshelf, 2 peak, 3 highshelf, 4 highcut, 5 notch, 6 bandpass, 7 tilt
    pub kind: u8,
    pub f: f32,
    pub g: f32,
    pub q: f32,
    /// biquad stages for cut filters (1..4)
    pub stages: u8,
    /// 0 stereo, 1 left, 2 right, 3 mid, 4 side
    pub place: u8,
    pub dynamic: bool,
    pub range: f32,
    pub thr: f32,
}

/// While alive, the calling thread treats denormal floats as zero (x86-64: MXCSR FTZ + DAZ,
/// AArch64: FPCR FZ); the previous mode comes back on drop.
///
/// Every recursive filter, envelope follower and feedback line decays towards zero after its
/// input stops, and passes through the denormal range (below ~1e-38) on the way. Arithmetic on
/// denormals is done in microcode, tens of times slower than normal floats: without this, the
/// seconds after a phrase ends (the chain is not parked until its tail is over) are the most
/// expensive blocks of a song, in every effect at once. The values dropped are 760 dB down.
pub struct NoDenormals(#[allow(dead_code)] u64);

impl NoDenormals {
    #[inline]
    pub fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            let mut old: u32 = 0;
            // SAFETY: stmxcsr/ldmxcsr read and write the calling thread's SSE control register
            // through a valid pointer to a u32; SSE is part of the x86-64 baseline.
            unsafe {
                std::arch::asm!("stmxcsr [{}]", in(reg) &mut old, options(nostack, preserves_flags));
                let new: u32 = old | 0x8040; // FTZ (bit 15) | DAZ (bit 6)
                std::arch::asm!("ldmxcsr [{}]", in(reg) &new, options(nostack, preserves_flags, readonly));
            }
            Self(old as u64)
        }
        #[cfg(target_arch = "aarch64")]
        {
            let old: u64;
            // SAFETY: FPCR is the calling thread's floating-point control register, readable and
            // writable at EL0.
            unsafe {
                std::arch::asm!("mrs {}, fpcr", out(reg) old, options(nomem, nostack, preserves_flags));
                std::arch::asm!("msr fpcr, {}", in(reg) old | (1 << 24), options(nomem, nostack, preserves_flags));
            }
            Self(old)
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        Self(0)
    }
}

impl Default for NoDenormals {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NoDenormals {
    #[inline]
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        {
            let old = self.0 as u32;
            // SAFETY: as in `new`; restores the value read there.
            unsafe { std::arch::asm!("ldmxcsr [{}]", in(reg) &old, options(nostack, preserves_flags, readonly)) };
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: as in `new`; restores the value read there.
        unsafe {
            std::arch::asm!("msr fpcr, {}", in(reg) self.0, options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NoDenormals;

    #[test]
    fn denormals_are_flushed_only_while_the_guard_lives() {
        let tiny = std::hint::black_box(1.0e-30f32);
        let scale = std::hint::black_box(1.0e-10f32);
        assert!((tiny * scale) > 0.0, "a denormal result outside the guard");
        {
            let _g = NoDenormals::new();
            if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
                assert_eq!(std::hint::black_box(tiny) * std::hint::black_box(scale), 0.0);
            }
        }
        assert!((std::hint::black_box(tiny) * std::hint::black_box(scale)) > 0.0, "the mode is restored");
    }
}
