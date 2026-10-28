//! Small realtime-safe effects used around the pitch shifter.

use std::f32::consts::PI;

/// Converts a time constant to a one-pole smoothing coefficient.
fn coeff(sample_rate: u32, ms: f32) -> f32 {
    1.0 - (-1.0 / (sample_rate as f32 * ms / 1000.0)).exp()
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
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

/// RBJ cookbook low-shelf biquad (S = 1).
pub struct LowShelf {
    sample_rate: u32,
    freq: f32,
    gain_db: f32,
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl LowShelf {
    pub fn new(sample_rate: u32, freq: f32, gain_db: f32) -> Self {
        let mut s = Self {
            sample_rate,
            freq,
            gain_db: f32::NAN,
            b: [1.0, 0.0, 0.0],
            a: [0.0, 0.0],
            x: [0.0; 2],
            y: [0.0; 2],
        };
        s.set_gain_db(gain_db);
        s
    }

    pub fn set_gain_db(&mut self, gain_db: f32) {
        if gain_db == self.gain_db {
            return;
        }
        self.gain_db = gain_db;
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * self.freq / self.sample_rate as f32;
        let (sin, cos) = w0.sin_cos();
        // With shelf slope S = 1: alpha = sin(w0)/2 * sqrt(2).
        let alpha = sin / 2.0 * 2f32.sqrt();
        let sq = 2.0 * a.sqrt() * alpha;

        let b0 = a * ((a + 1.0) - (a - 1.0) * cos + sq);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos - sq);
        let a0 = (a + 1.0) + (a - 1.0) * cos + sq;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos);
        let a2 = (a + 1.0) + (a - 1.0) * cos - sq;

        self.b = [b0 / a0, b1 / a0, b2 / a0];
        self.a = [a1 / a0, a2 / a0];
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

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn steady_gain(shelf: &mut LowShelf, freq: f32) -> f32 {
        let n = SR as usize;
        let mut peak = 0f32;
        for i in 0..n {
            let x = (2.0 * PI * freq * i as f32 / SR as f32).sin();
            let y = shelf.process(x);
            if i > n / 2 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn low_shelf_boosts_lows_not_highs() {
        let mut shelf = LowShelf::new(SR, 200.0, 12.0);
        let low = steady_gain(&mut shelf, 40.0);
        let mut shelf = LowShelf::new(SR, 200.0, 12.0);
        let high = steady_gain(&mut shelf, 5000.0);
        assert!((low - db_to_gain(12.0)).abs() < 0.4, "low gain {low}");
        assert!((high - 1.0).abs() < 0.05, "high gain {high}");
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
