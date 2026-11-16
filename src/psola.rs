//! Pitch-synchronous overlap-add (TD-PSOLA) pitch shifter with independent formant control.
//!
//! The granular shifter in `pitch.rs` resamples everything, so a big downward shift drags the
//! vocal-tract resonances (formants) down with the pitch and the result sounds like slowed tape.
//! Here the input is cut into Hann-windowed grains two pitch periods long, centred on pitch
//! marks, and the grains are laid out again at the new period spacing. Each grain keeps its
//! original waveform, and with it the spectral envelope, so the formants stay where they were
//! unless the grain itself is resampled, which is exactly what the formant control does.
//!
//! A pitch tracker runs on the input every `HOP_MS`; between voiced stretches (silence,
//! consonants) a fixed pseudo-period is used and the pitch ratio is forced to 1.

use std::f32::consts::PI;

use crate::analysis;

const MIN_HZ: f32 = 60.0;
const MAX_HZ: f32 = 400.0;
const HOP_MS: f32 = 5.0;
const ANALYSIS_MS: f32 = 30.0;
/// Output lags input by this much: room for a grain's right half (one max period, 16.7 ms)
/// plus the tracker's look-back.
pub const DELAY_MS: f32 = 35.0;
/// Period used for unvoiced or silent input.
const UNVOICED_PERIOD_MS: f32 = 10.0;
/// Hops the last voiced period is kept after the tracker loses it, so a momentary dropout
/// inside a vowel doesn't snap the pitch back to the original.
const VOICED_HOLD_HOPS: u32 = 4;
/// Worst case overlap: +12 st pitch with -12 st formant gives 2 * 2 / 0.5 = 8 grains.
const MAX_GRAINS: usize = 8;
/// Recent analysis marks kept for the synthesis side to pick from.
const MARKS: usize = 32;
const HANN_TABLE: usize = 1024;

#[derive(Clone, Copy)]
struct Grain {
    /// Input-time index of the pitch mark the grain is centred on.
    center: u64,
    /// One pitch period in samples: the grain spans `center ± half`.
    half: f32,
    /// Read speed in input samples per output sample (the formant ratio).
    step: f32,
    /// Current read offset from `center`, from `-half` to `+half`.
    pos: f32,
}

pub struct PsolaShifter {
    sample_rate: u32,
    buf: Vec<f32>,
    mask: u64,
    /// Absolute index of the next input sample; `write & mask` is its slot.
    write: u64,
    delay: u64,
    ratio: f32,
    formant: f32,
    hann: Vec<f32>,

    // Pitch tracker.
    hop: u32,
    hop_count: u32,
    scratch: Vec<f32>,
    scores: Vec<f32>,
    period: f32,
    unvoiced_period: f32,
    voiced_hold: u32,

    // Analysis marks, a ring of absolute input indices; `mark_head` is the newest.
    marks: [u64; MARKS],
    mark_head: usize,

    // Synthesis.
    next_out_mark: f64,
    grains: [Option<Grain>; MAX_GRAINS],
}

impl PsolaShifter {
    pub fn new(sample_rate: u32, semitones: f32, formant_semitones: f32) -> Self {
        let sr = sample_rate as f32;
        let ms = |ms: f32| (sr * ms / 1000.0) as usize;
        let max_period = ms(1000.0 / MIN_HZ);
        // Oldest read is a grain's left edge: DELAY + 1.5 periods behind the write head.
        let size = (ms(DELAY_MS) + 3 * max_period).next_power_of_two();
        let mut s = Self {
            sample_rate,
            buf: vec![0.0; size],
            mask: size as u64 - 1,
            write: 0,
            delay: ms(DELAY_MS) as u64,
            ratio: 1.0,
            formant: 1.0,
            hann: (0..HANN_TABLE)
                .map(|i| {
                    let s = (PI * i as f32 / (HANN_TABLE - 1) as f32).sin();
                    s * s
                })
                .collect(),
            hop: ms(HOP_MS) as u32,
            hop_count: 0,
            scratch: vec![0.0; ms(ANALYSIS_MS)],
            scores: vec![0.0; max_period + 2],
            period: ms(UNVOICED_PERIOD_MS) as f32,
            unvoiced_period: ms(UNVOICED_PERIOD_MS) as f32,
            voiced_hold: 0,
            marks: [0; MARKS],
            mark_head: 0,
            next_out_mark: 0.0,
            grains: [None; MAX_GRAINS],
        };
        s.set_semitones(semitones);
        s.set_formant(formant_semitones);
        s
    }

    pub fn set_semitones(&mut self, semitones: f32) {
        self.ratio = 2f32.powf(semitones / 12.0);
    }

    /// Formant shift in semitones relative to natural; 0 leaves the vowels alone.
    pub fn set_formant(&mut self, semitones: f32) {
        self.formant = 2f32.powf(semitones / 12.0);
    }

    fn voiced(&self) -> bool {
        self.voiced_hold > 0
    }

    /// Pitch ratio actually applied right now (1 while unvoiced).
    fn effective_ratio(&self) -> f32 {
        if self.voiced() { self.ratio } else { 1.0 }
    }

    /// `mix` blends the shifted signal (1.0) with the original (0.0).
    pub fn process_sample(&mut self, input: f32, mix: f32) -> f32 {
        let w = self.write;
        self.buf[(w & self.mask) as usize] = input;

        self.hop_count += 1;
        if self.hop_count >= self.hop {
            self.hop_count = 0;
            self.track();
        }
        self.place_mark();

        // Output time in input coordinates.
        let tau = w as f64 - self.delay as f64;
        if self.next_out_mark < tau - 2.0 * self.period as f64 {
            self.next_out_mark = tau;
        }
        while tau >= self.next_out_mark {
            self.start_grain(self.next_out_mark);
            self.next_out_mark += (self.period / self.effective_ratio()) as f64;
        }

        let (buf, hann, mask) = (&self.buf, &self.hann, self.mask);
        let mut wet = 0.0;
        for slot in &mut self.grains {
            if let Some(g) = slot {
                let phase = (g.pos + g.half) / (2.0 * g.half);
                let window = hann[(phase * (HANN_TABLE - 1) as f32) as usize];
                let p = g.pos.floor();
                let frac = g.pos - p;
                let i0 = g.center.wrapping_add_signed(p as i64);
                let newer = buf[(i0 & mask) as usize];
                let older = buf[(i0.wrapping_add(1) & mask) as usize];
                wet += (newer + (older - newer) * frac) * window;
                g.pos += g.step;
                if g.pos > g.half {
                    *slot = None;
                }
            }
        }
        // Grains overlap 2:1 at unity. Less overlap thins the sum (and adds modulation at the
        // new pitch, which is the point), more overlap thickens it; compensate the average.
        let overlap = self.effective_ratio() / self.formant;
        wet *= if overlap >= 1.0 { 1.0 / overlap } else { 1.0 / overlap.sqrt() };

        let dry = self.buf[(w.wrapping_sub(self.delay) & self.mask) as usize];
        self.write = w.wrapping_add(1);
        if mix < 1.0 { dry + (wet - dry) * mix } else { wet }
    }

    /// Re-estimates the period from the most recent `ANALYSIS_MS` of input.
    fn track(&mut self) {
        let n = self.scratch.len() as u64;
        let start = self.write.wrapping_add(1).wrapping_sub(n);
        for (i, s) in self.scratch.iter_mut().enumerate() {
            *s = self.buf[(start.wrapping_add(i as u64) & self.mask) as usize];
        }
        match analysis::track_pitch(&self.scratch, self.sample_rate, MIN_HZ, MAX_HZ, &mut self.scores) {
            Some(f0) => {
                self.period = self.sample_rate as f32 / f0;
                self.voiced_hold = VOICED_HOLD_HOPS;
            }
            None if self.voiced_hold > 0 => {
                self.voiced_hold -= 1;
                if self.voiced_hold == 0 {
                    self.period = self.unvoiced_period;
                }
            }
            None => {}
        }
    }

    /// Drops the next analysis mark one period after the last, snapped to the loudest sample
    /// within a quarter period either side so grains line up on the same point of each cycle.
    fn place_mark(&mut self) {
        let radius = (self.period * 0.25) as u64;
        let predicted = self.marks[self.mark_head] + self.period as u64;
        if self.write < predicted + radius {
            return;
        }
        let mut best = predicted;
        let mut best_level = -1.0;
        for i in (predicted - radius)..=(predicted + radius) {
            let level = self.buf[(i & self.mask) as usize].abs();
            if level > best_level {
                best_level = level;
                best = i;
            }
        }
        self.mark_head = (self.mark_head + 1) % MARKS;
        self.marks[self.mark_head] = best;
    }

    /// Starts a grain on the analysis mark nearest to output time `tau`.
    fn start_grain(&mut self, tau: f64) {
        let center = self
            .marks
            .iter()
            .copied()
            .min_by(|&a, &b| (a as f64 - tau).abs().total_cmp(&(b as f64 - tau).abs()))
            .unwrap_or(0);
        let half = self.period;
        let grain = Grain { center, half, step: self.formant, pos: -half };
        // Take a free slot, else evict the grain closest to finishing.
        let slot = (0..MAX_GRAINS)
            .find(|&i| self.grains[i].is_none())
            .or_else(|| {
                (0..MAX_GRAINS).max_by(|&a, &b| {
                    let f = |i: usize| self.grains[i].map_or(f32::MAX, |g| g.pos / g.half);
                    f(a).total_cmp(&f(b))
                })
            })
            .unwrap_or(0);
        self.grains[slot] = Some(grain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{Spectrum, detect_pitch};
    use crate::pitch::PitchShifter;

    const SR: u32 = 48_000;

    fn sine(freq: f32, secs: f32) -> Vec<f32> {
        let n = (SR as f32 * secs) as usize;
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f32 / SR as f32).sin() * 0.5)
            .collect()
    }

    /// A vowel-like signal: an impulse train at `f0` through a resonator at `formant_hz`.
    fn vowel(f0: f32, formant_hz: f32, secs: f32) -> Vec<f32> {
        let n = (SR as f32 * secs) as usize;
        let period = SR as f32 / f0;
        let bw = 80.0;
        let r = (-PI * bw / SR as f32).exp();
        let theta = 2.0 * PI * formant_hz / SR as f32;
        let (a1, a2) = (2.0 * r * theta.cos(), -r * r);
        let (mut y1, mut y2) = (0.0f32, 0.0f32);
        let mut next_pulse = 0.0f32;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let mut x = 0.0;
            if i as f32 >= next_pulse {
                x = 1.0;
                next_pulse += period;
            }
            let y = x + a1 * y1 + a2 * y2;
            y2 = y1;
            y1 = y;
            out.push(y);
        }
        let peak = out.iter().fold(0f32, |m, v| m.max(v.abs()));
        out.iter().map(|v| v / peak * 0.5).collect()
    }

    fn shift(input: &[f32], semitones: f32, formant: f32) -> Vec<f32> {
        let mut ps = PsolaShifter::new(SR, semitones, formant);
        input.iter().map(|&x| ps.process_sample(x, 1.0)).collect()
    }

    /// Pitch of the last 4096 samples, well after the tracker and delay have settled.
    fn tail_pitch(signal: &[f32]) -> f32 {
        detect_pitch(&signal[signal.len() - 4096..], SR).expect("voiced")
    }

    /// Centre frequency of the loudest spectrum band in the tail.
    fn tail_peak_hz(signal: &[f32]) -> f32 {
        let (lo, hi, bands) = (100.0, 3200.0, 120);
        let b = Spectrum::new(SR).bands(signal, lo, hi, bands);
        let loudest = b.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        lo * (hi / lo).powf((loudest as f32 + 0.5) / bands as f32)
    }

    fn assert_close(got: f32, expected: f32, tolerance: f32, what: &str) {
        assert!((got - expected).abs() <= expected * tolerance, "{what}: got {got}, expected {expected}");
    }

    #[test]
    fn octave_down_halves_frequency() {
        assert_close(tail_pitch(&shift(&sine(200.0, 2.0), -12.0, 0.0)), 100.0, 0.03, "pitch");
    }

    #[test]
    fn three_semitones_down() {
        let expected = 200.0 * 2f32.powf(-3.0 / 12.0);
        assert_close(tail_pitch(&shift(&sine(200.0, 2.0), -3.0, 0.0)), expected, 0.03, "pitch");
    }

    #[test]
    fn zero_shift_keeps_frequency() {
        assert_close(tail_pitch(&shift(&sine(200.0, 2.0), 0.0, 0.0)), 200.0, 0.03, "pitch");
    }

    #[test]
    fn output_is_finite_and_bounded() {
        let out = shift(&sine(150.0, 1.0), -5.0, -3.0);
        assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    }

    #[test]
    fn changing_pitch_and_formant_live_stays_bounded() {
        let input = vowel(120.0, 600.0, 1.0);
        let mut ps = PsolaShifter::new(SR, -3.0, 0.0);
        for (i, &x) in input.iter().enumerate() {
            if i % 4800 == 0 {
                ps.set_semitones(-12.0 + (i / 4800) as f32 * 2.4);
                ps.set_formant(-12.0 + (i / 4800) as f32 * 2.4);
            }
            let y = ps.process_sample(x, 0.5);
            assert!(y.is_finite() && y.abs() <= 1.5, "sample {i} = {y}");
        }
    }

    /// The reason this module exists: a 7-semitone drop keeps the vowel's resonance in place,
    /// where the granular shifter drags it down with the pitch.
    #[test]
    fn formants_survive_a_big_shift() {
        let input = vowel(110.0, 700.0, 2.0);
        assert_close(tail_peak_hz(&input), 700.0, 0.15, "input peak");

        let out = shift(&input, -7.0, 0.0);
        assert_close(tail_pitch(&out), 110.0 * 2f32.powf(-7.0 / 12.0), 0.04, "psola pitch");
        assert_close(tail_peak_hz(&out), 700.0, 0.15, "psola peak");

        let mut grain = PitchShifter::new(SR, -7.0, 40.0);
        let granular: Vec<f32> = input.iter().map(|&x| grain.process_sample(x, 1.0)).collect();
        let moved = tail_peak_hz(&granular);
        assert!(moved < 560.0, "granular peak should have dropped toward 467 Hz, got {moved}");
    }

    #[test]
    fn formant_knob_moves_envelope() {
        let out = shift(&vowel(110.0, 700.0, 2.0), -7.0, -5.0);
        assert_close(tail_peak_hz(&out), 700.0 * 2f32.powf(-5.0 / 12.0), 0.15, "shifted peak");
    }
}
