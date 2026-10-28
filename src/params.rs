//! Parameters shared between the UI and the audio callback.
//!
//! Everything is a lock-free atomic so the audio thread never blocks on the UI.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

#[derive(Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

/// A continuously adjustable parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Knob {
    Pitch,
    Mix,
    Warmth,
    Gate,
    Gain,
    Window,
}

pub struct KnobSpec {
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub step: f32,
    pub fine_step: f32,
    pub help: &'static str,
}

/// Gate thresholds at or below this are treated as "off".
pub const GATE_OFF_DB: f32 = -80.0;

impl Knob {
    pub const ALL: [Knob; 6] = [
        Knob::Pitch,
        Knob::Mix,
        Knob::Warmth,
        Knob::Gate,
        Knob::Gain,
        Knob::Window,
    ];

    pub fn spec(self) -> KnobSpec {
        let s = |label, min, max, default, step, fine_step, help| KnobSpec {
            label,
            min,
            max,
            default,
            step,
            fine_step,
            help,
        };
        match self {
            Knob::Pitch => s("Pitch", -12.0, 12.0, -3.0, 0.5, 0.1, "semitones; negative is deeper"),
            Knob::Mix => s("Mix", 0.0, 1.0, 1.0, 0.05, 0.01, "shifted voice vs. original"),
            Knob::Warmth => s("Warmth", 0.0, 12.0, 0.0, 1.0, 0.5, "low-shelf bass boost at 200 Hz"),
            Knob::Gate => s("Gate", GATE_OFF_DB, -20.0, GATE_OFF_DB, 2.0, 1.0, "silence input below this level"),
            Knob::Gain => s("Gain", -20.0, 12.0, 0.0, 0.5, 0.1, "output level"),
            Knob::Window => s("Window", 15.0, 100.0, 40.0, 5.0, 1.0, "grain size; longer is smoother, adds latency"),
        }
    }

    pub fn format(self, v: f32) -> String {
        match self {
            Knob::Pitch => format!("{v:+.1} st"),
            Knob::Mix => format!("{:.0} %", v * 100.0),
            Knob::Warmth => format!("{v:+.1} dB"),
            Knob::Gate if v <= GATE_OFF_DB => "off".into(),
            Knob::Gate => format!("{v:.0} dB"),
            Knob::Gain => format!("{v:+.1} dB"),
            Knob::Window => format!("{v:.0} ms"),
        }
    }
}

pub struct Params {
    pitch: AtomicF32,
    mix: AtomicF32,
    warmth: AtomicF32,
    gate: AtomicF32,
    gain: AtomicF32,
    window: AtomicF32,
    pub bypass: AtomicBool,
    pub mute: AtomicBool,
}

impl Params {
    pub fn new() -> Self {
        Self {
            pitch: AtomicF32::new(Knob::Pitch.spec().default),
            mix: AtomicF32::new(Knob::Mix.spec().default),
            warmth: AtomicF32::new(Knob::Warmth.spec().default),
            gate: AtomicF32::new(Knob::Gate.spec().default),
            gain: AtomicF32::new(Knob::Gain.spec().default),
            window: AtomicF32::new(Knob::Window.spec().default),
            bypass: AtomicBool::new(false),
            mute: AtomicBool::new(false),
        }
    }

    fn slot(&self, knob: Knob) -> &AtomicF32 {
        match knob {
            Knob::Pitch => &self.pitch,
            Knob::Mix => &self.mix,
            Knob::Warmth => &self.warmth,
            Knob::Gate => &self.gate,
            Knob::Gain => &self.gain,
            Knob::Window => &self.window,
        }
    }

    pub fn get(&self, knob: Knob) -> f32 {
        self.slot(knob).get()
    }

    pub fn set(&self, knob: Knob, v: f32) {
        let spec = knob.spec();
        self.slot(knob).set(v.clamp(spec.min, spec.max));
    }

    pub fn nudge(&self, knob: Knob, delta: f32) {
        // Round to the fine step so repeated nudges don't accumulate float error.
        let fine = knob.spec().fine_step;
        let v = ((self.get(knob) + delta) / fine).round() * fine;
        self.set(knob, v);
    }

    pub fn toggle(flag: &AtomicBool) {
        flag.fetch_xor(true, Ordering::Relaxed);
    }
}

/// Counters and state reported back from the audio callbacks.
#[derive(Default)]
pub struct Stats {
    pub dropped: AtomicU64,
    pub underruns: AtomicU64,
    pub resyncs: AtomicU64,
    pub overloads: AtomicU64,
    pub gate_open: AtomicBool,
    /// Most recent non-overload stream error. Only touched from error callbacks, never the
    /// realtime data path.
    pub last_error: Mutex<Option<String>>,
}

impl Stats {
    pub fn glitches(&self) -> [u64; 4] {
        [
            self.dropped.load(Ordering::Relaxed),
            self.underruns.load(Ordering::Relaxed),
            self.resyncs.load(Ordering::Relaxed),
            self.overloads.load(Ordering::Relaxed),
        ]
    }
}
