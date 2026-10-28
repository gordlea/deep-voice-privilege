//! Real-time granular pitch shifter.
//!
//! Two read taps sweep through a short circular delay line at `ratio` times the
//! write speed. Each tap's delay ramps across a window and then wraps; the taps are
//! half a window apart and Hann-weighted, so one is always silent when the other wraps
//! and the sum of their gains is exactly 1.

use std::f32::consts::PI;

/// The delay line is sized for this so the window can change without reallocating.
pub const MAX_WINDOW_MS: f32 = 120.0;

pub struct PitchShifter {
    sample_rate: f32,
    buf: Vec<f32>,
    mask: usize,
    write: usize,
    window: f32,
    ratio: f32,
    /// Position of tap A within the window, in [0, 1). Tap B sits at `phase + 0.5`.
    phase: f32,
    phase_step: f32,
}

impl PitchShifter {
    /// `window_ms` trades smoothness (longer) against latency and echo (shorter).
    pub fn new(sample_rate: u32, semitones: f32, window_ms: f32) -> Self {
        let sample_rate = sample_rate as f32;
        // +2 leaves room for the 1-sample minimum delay and the interpolation neighbour.
        let size = ((sample_rate * MAX_WINDOW_MS / 1000.0) as usize + 2).next_power_of_two();
        let mut s = Self {
            sample_rate,
            buf: vec![0.0; size],
            mask: size - 1,
            write: 0,
            window: 0.0,
            ratio: 1.0,
            phase: 0.0,
            phase_step: 0.0,
        };
        s.set_window_ms(window_ms);
        s.set_semitones(semitones);
        s
    }

    pub fn set_semitones(&mut self, semitones: f32) {
        self.ratio = 2f32.powf(semitones / 12.0);
        self.phase_step = (1.0 - self.ratio) / self.window;
    }

    pub fn set_window_ms(&mut self, window_ms: f32) {
        let window_ms = window_ms.min(MAX_WINDOW_MS);
        self.window = (self.sample_rate * window_ms / 1000.0).max(64.0);
        self.phase_step = (1.0 - self.ratio) / self.window;
    }

    /// `mix` blends the shifted signal (1.0) with the original (0.0). The original is
    /// delayed by half a window to line up with the taps' average delay, which keeps the
    /// blend from sounding like a flanger.
    pub fn process_sample(&mut self, input: f32, mix: f32) -> f32 {
        self.buf[self.write] = input;

        let pa = self.phase;
        let pb = (pa + 0.5).fract();
        let wet = self.tap(pa) * hann(pa) + self.tap(pb) * hann(pb);
        let out = if mix < 1.0 {
            let dry = self.tap(0.5);
            dry + (wet - dry) * mix
        } else {
            wet
        };

        self.phase = (self.phase + self.phase_step).rem_euclid(1.0);
        self.write = (self.write + 1) & self.mask;
        out
    }

    /// Linearly interpolated read `1 + phase * window` samples behind the write head.
    fn tap(&self, phase: f32) -> f32 {
        let delay = 1.0 + phase * self.window;
        let d = delay.floor();
        let frac = delay - d;
        // Newer sample is `d` behind, older is `d + 1` behind.
        let newer = self.buf[(self.write.wrapping_sub(d as usize)) & self.mask];
        let older = self.buf[(self.write.wrapping_sub(d as usize + 1)) & self.mask];
        newer + (older - newer) * frac
    }
}

fn hann(phase: f32) -> f32 {
    let s = (PI * phase).sin();
    s * s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn sine(freq: f32, secs: f32) -> Vec<f32> {
        let n = (SR as f32 * secs) as usize;
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f32 / SR as f32).sin() * 0.5)
            .collect()
    }

    fn shift(input: &[f32], semitones: f32) -> Vec<f32> {
        let mut ps = PitchShifter::new(SR, semitones, 40.0);
        input.iter().map(|&x| ps.process_sample(x, 1.0)).collect()
    }

    /// Estimates frequency from upward zero crossings, skipping the warm-up.
    fn measure_freq(signal: &[f32]) -> f32 {
        let s = &signal[SR as usize / 10..];
        let crossings = s.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        crossings as f32 * SR as f32 / s.len() as f32
    }

    #[test]
    fn octave_down_halves_frequency() {
        let f = measure_freq(&shift(&sine(440.0, 2.0), -12.0));
        assert!((f - 220.0).abs() < 11.0, "measured {f} Hz");
    }

    #[test]
    fn three_semitones_down() {
        let expected = 440.0 * 2f32.powf(-3.0 / 12.0);
        let f = measure_freq(&shift(&sine(440.0, 2.0), -3.0));
        assert!((f - expected).abs() < expected * 0.05, "measured {f} Hz, expected {expected}");
    }

    #[test]
    fn zero_shift_keeps_frequency() {
        let f = measure_freq(&shift(&sine(440.0, 2.0), 0.0));
        assert!((f - 440.0).abs() < 22.0, "measured {f} Hz");
    }

    #[test]
    fn changing_window_and_pitch_live_stays_bounded() {
        let input = sine(200.0, 1.0);
        let mut ps = PitchShifter::new(SR, -3.0, 40.0);
        for (i, &x) in input.iter().enumerate() {
            if i % 4800 == 0 {
                ps.set_window_ms(15.0 + (i / 4800) as f32 * 10.0);
                ps.set_semitones(-12.0 + (i / 4800) as f32 * 2.0);
            }
            let y = ps.process_sample(x, 0.5);
            assert!(y.is_finite() && y.abs() <= 0.5 + 1e-4);
        }
    }

    #[test]
    fn output_is_finite_and_bounded() {
        let out = shift(&sine(300.0, 1.0), -5.0);
        assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 0.5 + 1e-4));
    }
}
