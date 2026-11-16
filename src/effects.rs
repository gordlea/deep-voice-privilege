//! Small realtime-safe effects used around the pitch shifter.

use std::f32::consts::PI;

/// Converts a time constant to a one-pole smoothing coefficient.
fn coeff(sample_rate: u32, ms: f32) -> f32 {
    1.0 - (-1.0 / (sample_rate as f32 * ms / 1000.0)).exp()
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(1e-6).log10()
}

/// One-pole smoother that removes zipper noise when a control jumps.
pub struct Smoother {
    value: f32,
    k: f32,
}

impl Smoother {
    pub fn new(sample_rate: u32, ms: f32, initial: f32) -> Self {
        Self { value: initial, k: coeff(sample_rate, ms) }
    }

    pub fn next(&mut self, target: f32) -> f32 {
        self.value += (target - self.value) * self.k;
        self.value
    }
}

/// Noise gate: an envelope follower drives a smoothed open/closed gain.
pub struct Gate {
    env: f32,
    gain: f32,
    env_attack: f32,
    env_release: f32,
    open_k: f32,
    close_k: f32,
}

impl Gate {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            env: 0.0,
            gain: 1.0,
            env_attack: coeff(sample_rate, 1.0),
            env_release: coeff(sample_rate, 150.0),
            open_k: coeff(sample_rate, 2.0),
            close_k: coeff(sample_rate, 80.0),
        }
    }

    /// `threshold` is linear amplitude; 0 disables the gate.
    pub fn process(&mut self, x: f32, threshold: f32) -> f32 {
        let level = x.abs();
        let k = if level > self.env { self.env_attack } else { self.env_release };
        self.env += (level - self.env) * k;

        let (target, k) = if self.env >= threshold { (1.0, self.open_k) } else { (0.0, self.close_k) };
        self.gain += (target - self.gain) * k;
        x * self.gain
    }

    pub fn is_open(&self) -> bool {
        self.gain > 0.5
    }
}

/// Gentle feed-forward compressor that evens out close-mic speech. One control (the
/// threshold); ratio, knee and timing are fixed and the makeup gain follows the threshold.
pub struct Compressor {
    env: f32,
    gain_db: f32,
    attack: f32,
    release: f32,
}

const COMP_RATIO: f32 = 3.0;
const COMP_KNEE_DB: f32 = 6.0;
/// Fraction of the theoretical full makeup applied; full makeup makes quiet rooms roar.
const COMP_MAKEUP: f32 = 0.6;

impl Compressor {
    pub fn new(sample_rate: u32) -> Self {
        Self { env: 0.0, gain_db: 0.0, attack: coeff(sample_rate, 5.0), release: coeff(sample_rate, 120.0) }
    }

    /// Gain reduction in dB for a level `over` dB above threshold, with a soft knee.
    fn reduction_db(over: f32) -> f32 {
        let slope = 1.0 - 1.0 / COMP_RATIO;
        let half = COMP_KNEE_DB / 2.0;
        if over <= -half {
            0.0
        } else if over >= half {
            over * slope
        } else {
            slope * (over + half) * (over + half) / (2.0 * COMP_KNEE_DB)
        }
    }

    pub fn makeup_db(threshold_db: f32) -> f32 {
        -threshold_db * (1.0 - 1.0 / COMP_RATIO) * COMP_MAKEUP
    }

    /// `threshold_db` at or above 0 dB disables the compressor.
    pub fn process(&mut self, x: f32, threshold_db: f32) -> f32 {
        if threshold_db >= 0.0 {
            self.env = 0.0;
            self.gain_db = 0.0;
            return x;
        }
        let level = x.abs();
        let k = if level > self.env { self.attack } else { self.release };
        self.env += (level - self.env) * k;

        let target = Self::makeup_db(threshold_db) - Self::reduction_db(gain_to_db(self.env) - threshold_db);
        // Clamp down fast, recover slowly.
        let k = if target < self.gain_db { self.attack } else { self.release };
        self.gain_db += (target - self.gain_db) * k;
        x * db_to_gain(self.gain_db)
    }
}

/// Direct-form I biquad; the coefficient builders below fill it from the RBJ cookbook.
pub struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl Biquad {
    fn identity() -> Self {
        Self { b: [1.0, 0.0, 0.0], a: [0.0, 0.0], x: [0.0; 2], y: [0.0; 2] }
    }

    /// Normalizes by `a0` and installs the coefficients, keeping the filter state.
    fn set(&mut self, b: [f32; 3], a: [f32; 3]) {
        self.b = [b[0] / a[0], b[1] / a[0], b[2] / a[0]];
        self.a = [a[1] / a[0], a[2] / a[0]];
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// RBJ shelf coefficients with slope S = 1. `high` selects a high shelf, otherwise low.
fn shelf_coeffs(sample_rate: u32, freq: f32, gain_db: f32, high: bool) -> ([f32; 3], [f32; 3]) {
    let a = 10f32.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * freq / sample_rate as f32;
    let (sin, cos) = w0.sin_cos();
    // With shelf slope S = 1: alpha = sin(w0)/2 * sqrt(2).
    let alpha = sin / 2.0 * 2f32.sqrt();
    let sq = 2.0 * a.sqrt() * alpha;
    // The high shelf is the low shelf mirrored: cos(w0) changes sign, as do b1 and a1.
    let (cos, flip) = if high { (-cos, -1.0) } else { (cos, 1.0) };
    let b0 = a * ((a + 1.0) - (a - 1.0) * cos + sq);
    let b1 = flip * 2.0 * a * ((a - 1.0) - (a + 1.0) * cos);
    let b2 = a * ((a + 1.0) - (a - 1.0) * cos - sq);
    let a0 = (a + 1.0) + (a - 1.0) * cos + sq;
    let a1 = flip * -2.0 * ((a - 1.0) + (a + 1.0) * cos);
    let a2 = (a + 1.0) + (a - 1.0) * cos - sq;
    ([b0, b1, b2], [a0, a1, a2])
}

/// RBJ second-order Butterworth high-pass (Q = 1/√2).
fn high_pass_coeffs(sample_rate: u32, freq: f32) -> ([f32; 3], [f32; 3]) {
    let w0 = 2.0 * PI * freq / sample_rate as f32;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * 2f32.sqrt() / 2.0);
    let b = [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0];
    let a = [1.0 + alpha, -2.0 * cos, 1.0 - alpha];
    (b, a)
}

/// A shelf whose gain can change live; coefficients are only recomputed when it does.
pub struct Shelf {
    sample_rate: u32,
    freq: f32,
    high: bool,
    gain_db: f32,
    filter: Biquad,
}

impl Shelf {
    fn new(sample_rate: u32, freq: f32, gain_db: f32, high: bool) -> Self {
        let mut s = Self { sample_rate, freq, high, gain_db: f32::NAN, filter: Biquad::identity() };
        s.set_gain_db(gain_db);
        s
    }

    pub fn set_gain_db(&mut self, gain_db: f32) {
        if gain_db == self.gain_db {
            return;
        }
        self.gain_db = gain_db;
        let (b, a) = shelf_coeffs(self.sample_rate, self.freq, gain_db, self.high);
        self.filter.set(b, a);
    }

    pub fn process(&mut self, x: f32) -> f32 {
        self.filter.process(x)
    }
}

/// RBJ cookbook low shelf (S = 1): bass boost or cut below `freq`.
pub struct LowShelf(Shelf);

impl LowShelf {
    pub fn new(sample_rate: u32, freq: f32, gain_db: f32) -> Self {
        Self(Shelf::new(sample_rate, freq, gain_db, false))
    }

    pub fn set_gain_db(&mut self, gain_db: f32) {
        self.0.set_gain_db(gain_db)
    }

    pub fn process(&mut self, x: f32) -> f32 {
        self.0.process(x)
    }
}

/// RBJ cookbook high shelf (S = 1): treble boost or cut above `freq`.
pub struct HighShelf(Shelf);

impl HighShelf {
    pub fn new(sample_rate: u32, freq: f32, gain_db: f32) -> Self {
        Self(Shelf::new(sample_rate, freq, gain_db, true))
    }

    pub fn set_gain_db(&mut self, gain_db: f32) {
        self.0.set_gain_db(gain_db)
    }

    pub fn process(&mut self, x: f32) -> f32 {
        self.0.process(x)
    }
}

/// Fixed second-order high-pass, for rumble and plosives.
pub struct HighPass(Biquad);

impl HighPass {
    pub fn new(sample_rate: u32, freq: f32) -> Self {
        let mut f = Biquad::identity();
        let (b, a) = high_pass_coeffs(sample_rate, freq);
        f.set(b, a);
        Self(f)
    }

    pub fn process(&mut self, x: f32) -> f32 {
        self.0.process(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// Peak output of `f` over the second half of a one-second sine at `freq` with `amp`.
    fn steady_peak(mut f: impl FnMut(f32) -> f32, freq: f32, amp: f32) -> f32 {
        let n = SR as usize;
        let mut peak = 0f32;
        for i in 0..n {
            let x = (2.0 * PI * freq * i as f32 / SR as f32).sin() * amp;
            let y = f(x);
            if i > n / 2 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    fn steady_gain(mut f: impl FnMut(f32) -> f32, freq: f32) -> f32 {
        steady_peak(&mut f, freq, 1.0)
    }

    #[test]
    fn low_shelf_boosts_lows_not_highs() {
        let mut shelf = LowShelf::new(SR, 200.0, 12.0);
        let low = steady_gain(|x| shelf.process(x), 40.0);
        let mut shelf = LowShelf::new(SR, 200.0, 12.0);
        let high = steady_gain(|x| shelf.process(x), 5000.0);
        assert!((low - db_to_gain(12.0)).abs() < 0.4, "low gain {low}");
        assert!((high - 1.0).abs() < 0.05, "high gain {high}");
    }

    #[test]
    fn high_shelf_cuts_highs_not_lows() {
        let mut shelf = HighShelf::new(SR, 3500.0, -12.0);
        let high = steady_gain(|x| shelf.process(x), 14_000.0);
        let mut shelf = HighShelf::new(SR, 3500.0, -12.0);
        let low = steady_gain(|x| shelf.process(x), 100.0);
        let high_db = gain_to_db(high);
        assert!((high_db + 12.0).abs() < 2.5, "high gain {high_db} dB");
        assert!((low - 1.0).abs() < 0.05, "low gain {low}");
    }

    #[test]
    fn high_pass_removes_rumble() {
        let mut hpf = HighPass::new(SR, 70.0);
        let mid = steady_gain(|x| hpf.process(x), 1000.0);
        let mut hpf = HighPass::new(SR, 70.0);
        let rumble = steady_gain(|x| hpf.process(x), 20.0);
        assert!((mid - 1.0).abs() < 0.02, "1 kHz gain {mid}");
        assert!(gain_to_db(rumble) < -15.0, "20 Hz gain {} dB", gain_to_db(rumble));
    }

    #[test]
    fn compressor_squeezes_loud_and_lifts_quiet() {
        let threshold = -30.0;
        let mut comp = Compressor::new(SR);
        let loud_in = db_to_gain(-10.0);
        let loud = steady_peak(|x| comp.process(x, threshold), 1000.0, loud_in);
        assert!(loud < loud_in && loud > loud_in * 0.5, "loud peak {loud} for input {loud_in}");

        let mut comp = Compressor::new(SR);
        let quiet_in = db_to_gain(-50.0);
        let quiet = steady_peak(|x| comp.process(x, threshold), 1000.0, quiet_in);
        let expected = quiet_in * db_to_gain(Compressor::makeup_db(threshold));
        assert!((quiet - expected).abs() < expected * 0.1, "quiet peak {quiet}, expected {expected}");

        let mut comp = Compressor::new(SR);
        let off = steady_peak(|x| comp.process(x, 0.0), 1000.0, loud_in);
        assert!((off - loud_in).abs() < 1e-6, "off should be transparent, got {off}");
    }

    #[test]
    fn gate_closes_on_quiet_and_opens_on_loud() {
        let mut gate = Gate::new(SR);
        let threshold = db_to_gain(-40.0);
        for _ in 0..SR {
            gate.process(0.001, threshold);
        }
        assert!(!gate.is_open());
        for i in 0..SR / 10 {
            gate.process((i as f32 * 0.05).sin() * 0.3, threshold);
        }
        assert!(gate.is_open());
    }
}
