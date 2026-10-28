//! Signal analysis for the UI: spectrum, fundamental frequency and levels.
//!
//! Runs on the UI thread against snapshots copied out of the audio callback.

use std::f32::consts::PI;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

pub const FFT_SIZE: usize = 2048;

pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    scratch: Vec<Complex<f32>>,
    sample_rate: f32,
}

impl Spectrum {
    pub fn new(sample_rate: u32) -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        Self {
            fft,
            window,
            scratch: vec![Complex::default(); FFT_SIZE],
            sample_rate: sample_rate as f32,
        }
    }

    /// Returns `bands` dB magnitudes on a log-frequency axis from `lo` to `hi` Hz.
    /// `samples` must be at least `FFT_SIZE` long; the most recent samples are used.
    pub fn bands(&mut self, samples: &[f32], lo: f32, hi: f32, bands: usize) -> Vec<f32> {
        let s = &samples[samples.len() - FFT_SIZE..];
        for (c, (&x, &w)) in self.scratch.iter_mut().zip(s.iter().zip(&self.window)) {
            *c = Complex::new(x * w, 0.0);
        }
        self.fft.process(&mut self.scratch);

        let bin_hz = self.sample_rate / FFT_SIZE as f32;
        // Hann window coherent gain is 0.5, so a full-scale sine peaks at N/4.
        let norm = 4.0 / FFT_SIZE as f32;
        let ratio = (hi / lo).ln();
        (0..bands)
            .map(|b| {
                let f0 = lo * (ratio * b as f32 / bands as f32).exp();
                let f1 = lo * (ratio * (b + 1) as f32 / bands as f32).exp();
                let i0 = (f0 / bin_hz).floor() as usize;
                let i1 = ((f1 / bin_hz).ceil() as usize).max(i0 + 1).min(FFT_SIZE / 2);
                let peak = self.scratch[i0..i1]
                    .iter()
                    .map(|c| c.norm() * norm)
                    .fold(0.0, f32::max);
                to_db(peak)
            })
            .collect()
    }
}

pub fn to_db(amplitude: f32) -> f32 {
    20.0 * amplitude.max(1e-6).log10()
}

pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Estimates the fundamental of a voiced signal by normalized autocorrelation, searching
/// 60–500 Hz. Returns `None` for silence or noise.
pub fn detect_pitch(samples: &[f32], sample_rate: u32) -> Option<f32> {
    const MIN_HZ: f32 = 60.0;
    const MAX_HZ: f32 = 500.0;
    // Below about -40 dB it's room noise or mains hum, not a voice.
    if rms(samples) < 0.01 {
        return None;
    }
    let sr = sample_rate as f32;
    let min_lag = (sr / MAX_HZ) as usize;
    let max_lag = ((sr / MIN_HZ) as usize).min(samples.len() / 2);
    if min_lag >= max_lag {
        return None;
    }

    // Normalized correlation at each lag, so the threshold is independent of level.
    let n = samples.len() - max_lag;
    let corr = |lag: usize| {
        let (mut xy, mut xx, mut yy) = (0.0, 0.0, 0.0);
        for i in 0..n {
            let (a, b) = (samples[i], samples[i + lag]);
            xy += a * b;
            xx += a * a;
            yy += b * b;
        }
        xy / (xx * yy).sqrt().max(1e-12)
    };
    let scores: Vec<f32> = (min_lag..=max_lag).map(corr).collect();
    let best = scores.iter().copied().fold(f32::MIN, f32::max);
    if best < 0.6 {
        return None;
    }

    // Take the first peak close to the best one; later peaks are octave errors.
    let idx = (1..scores.len() - 1).find(|&i| {
        scores[i] >= best * 0.9 && scores[i] >= scores[i - 1] && scores[i] >= scores[i + 1]
    })?;

    // Parabolic interpolation around the peak for sub-sample lag precision.
    let (l, c, r) = (scores[idx - 1], scores[idx], scores[idx + 1]);
    let denom = l - 2.0 * c + r;
    let offset = if denom.abs() > 1e-9 { 0.5 * (l - r) / denom } else { 0.0 };
    Some(sr / (min_lag as f32 + idx as f32 + offset))
}

/// Converts a frequency to the nearest note name, e.g. `A3`.
pub fn note_name(freq: f32) -> String {
    const NAMES: [&str; 12] = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"];
    let midi = (69.0 + 12.0 * (freq / 440.0).log2()).round() as i32;
    format!("{}{}", NAMES[midi.rem_euclid(12) as usize], midi.div_euclid(12) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn tone(freq: f32, n: usize) -> Vec<f32> {
        // A fundamental plus harmonics, like a voice.
        (0..n)
            .map(|i| {
                let t = i as f32 / SR as f32;
                (1..=4)
                    .map(|h| (2.0 * PI * freq * h as f32 * t).sin() * 0.3 / h as f32)
                    .sum()
            })
            .collect()
    }

    #[test]
    fn detects_voice_like_pitch() {
        for f in [90.0, 140.0, 220.0, 330.0] {
            let got = detect_pitch(&tone(f, 4096), SR).expect("pitch");
            assert!((got - f).abs() < f * 0.02, "expected {f}, got {got}");
        }
    }

    #[test]
    fn silence_has_no_pitch() {
        assert_eq!(detect_pitch(&vec![0.0; 4096], SR), None);
    }

    #[test]
    fn spectrum_peaks_at_tone() {
        let mut spec = Spectrum::new(SR);
        let s: Vec<f32> = (0..FFT_SIZE)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / SR as f32).sin() * 0.5)
            .collect();
        let bands = spec.bands(&s, 100.0, 10_000.0, 40);
        let loudest = bands.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        // 1 kHz is halfway along a 100 Hz–10 kHz log axis.
        assert!((loudest as i32 - 20).abs() <= 1, "loudest band {loudest}");
        assert!((bands[loudest] - to_db(0.5)).abs() < 2.0, "level {}", bands[loudest]);
    }

    #[test]
    fn note_names() {
        assert_eq!(note_name(440.0), "A4");
        assert_eq!(note_name(130.81), "C3");
    }
}
