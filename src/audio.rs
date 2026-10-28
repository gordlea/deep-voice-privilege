//! Device setup and the realtime input → effects → output pipeline.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    Sample,
    BufferSize, Device, ErrorKind, FromSample, Host, SampleFormat, SizedSample, Stream, StreamConfig,
    SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::effects::{Gate, LowShelf, Smoother, db_to_gain};
use crate::params::{GATE_OFF_DB, Knob, Params, Stats};
use crate::pitch::PitchShifter;
use crate::platform;

/// Target fill of the input→output buffer, absorbing callback jitter.
const BUFFER_TARGET_MS: u32 = 20;
/// When the buffer exceeds this (clock drift between devices), skip back to the target.
const BUFFER_MAX_MS: u32 = 80;
/// How much (input, output) history the visualizer buffer can hold between UI frames.
const VIZ_BUFFER_MS: u32 = 500;
/// Requested audio callback period where the backend lets us choose (see `platform`).
const CALLBACK_MS: u32 = 10;
const WARMTH_SHELF_HZ: f32 = 200.0;

pub struct Engine {
    pub input_name: String,
    pub output_name: String,
    pub sample_rate: u32,
    pub params: Arc<Params>,
    pub stats: Arc<Stats>,
    /// (raw mic, processed output) sample pairs for visualization.
    pub viz: HeapCons<(f32, f32)>,
    _input_stream: Stream,
    _output_stream: Stream,
}

impl Engine {
    pub fn start(host: &Host, input: Option<&str>, output: &str, params: Arc<Params>) -> Result<Self> {
        let input = match input {
            Some(name) => find_device(host.input_devices()?, name)?,
            None => host
                .default_input_device()
                .context("no default input device")?,
        };
        let output = find_device(host.output_devices()?, output)
            .map_err(|e| anyhow!("{e}\n{}", platform::SETUP_HINT))?;

        let input_name = device_name(&input);
        let output_name = device_name(&output);
        if input_name == output_name {
            bail!("input and output are the same device ({input_name}); that would feed back");
        }

        // Run at the mic's native rate: capture devices rarely convert, playback ones usually do.
        // If the output can't do that rate, fall back to the output's rate if the mic allows it.
        let mut in_supported = input.default_input_config()?;
        let out_default = output.default_output_config()?;
        let out_ranges: Vec<_> = output.supported_output_configs()?.collect();
        let in_rate = in_supported.sample_rate();
        let out_supported = match pick_config(&out_ranges, out_default.channels(), in_rate) {
            Some(cfg) => cfg,
            None => {
                let out_rate = out_default.sample_rate();
                let in_ranges: Vec<_> = input.supported_input_configs()?.collect();
                match (
                    pick_config(&in_ranges, in_supported.channels(), out_rate),
                    pick_config(&out_ranges, out_default.channels(), out_rate),
                ) {
                    (Some(i), Some(o)) => {
                        in_supported = i;
                        o
                    }
                    _ => bail!(
                        "{output_name} can't run at {in_rate} Hz and {input_name} can't run at {out_rate} Hz. {}",
                        platform::RATE_HINT
                    ),
                }
            }
        };
        let rate = in_supported.sample_rate();
        let in_channels = in_supported.channels() as usize;
        let out_channels = out_supported.channels() as usize;

        let in_config = StreamConfig {
            channels: in_channels as u16,
            sample_rate: rate,
            buffer_size: buffer_size(&in_supported, rate),
        };
        let out_config = StreamConfig {
            channels: out_channels as u16,
            sample_rate: rate,
            buffer_size: buffer_size(&out_supported, rate),
        };

        let ms = |ms: u32| (rate * ms / 1000) as usize;
        let target = ms(BUFFER_TARGET_MS);
        let max = ms(BUFFER_MAX_MS);
        let (mut producer, consumer) = HeapRb::<f32>::new(max * 2).split();
        for _ in 0..target {
            let _ = producer.try_push(0.0);
        }
        let (viz_producer, viz) = HeapRb::<(f32, f32)>::new(ms(VIZ_BUFFER_MS)).split();

        let stats = Arc::new(Stats::default());

        let input_stream = match in_supported.sample_format() {
            SampleFormat::F32 => build_input::<f32>(&input, in_config, in_channels, producer, &stats),
            SampleFormat::F64 => build_input::<f64>(&input, in_config, in_channels, producer, &stats),
            SampleFormat::I16 => build_input::<i16>(&input, in_config, in_channels, producer, &stats),
            SampleFormat::I32 => build_input::<i32>(&input, in_config, in_channels, producer, &stats),
            SampleFormat::U8 => build_input::<u8>(&input, in_config, in_channels, producer, &stats),
            f => bail!("{input_name} uses {f} samples, which isn't supported"),
        }?;

        let dsp = Dsp::new(rate, params.clone(), consumer, viz_producer, stats.clone(), target, max);
        let output_stream = match out_supported.sample_format() {
            SampleFormat::F32 => build_output::<f32>(&output, out_config, out_channels, dsp, &stats),
            SampleFormat::F64 => build_output::<f64>(&output, out_config, out_channels, dsp, &stats),
            SampleFormat::I16 => build_output::<i16>(&output, out_config, out_channels, dsp, &stats),
            SampleFormat::I32 => build_output::<i32>(&output, out_config, out_channels, dsp, &stats),
            SampleFormat::U8 => build_output::<u8>(&output, out_config, out_channels, dsp, &stats),
            f => bail!("{output_name} uses {f} samples, which isn't supported"),
        }?;

        input_stream.play()?;
        output_stream.play()?;

        Ok(Self {
            input_name,
            output_name,
            sample_rate: rate,
            params,
            stats,
            viz,
            _input_stream: input_stream,
            _output_stream: output_stream,
        })
    }
}

/// Captures every channel, downmixes to mono f32 and queues it for the output callback.
fn build_input<T>(
    device: &Device,
    config: StreamConfig,
    channels: usize,
    mut producer: HeapProd<f32>,
    stats: &Arc<Stats>,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let in_stats = stats.clone();
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            for frame in data.chunks_exact(channels) {
                let mono = frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32;
                if producer.try_push(mono).is_err() {
                    in_stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        },
        stream_error_handler("input", stats.clone()),
        None,
    )?)
}

/// Pulls queued mic samples through the effects chain and writes them to every output channel.
fn build_output<T>(
    device: &Device,
    config: StreamConfig,
    channels: usize,
    mut dsp: Dsp,
    stats: &Arc<Stats>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            dsp.begin_buffer();
            for frame in data.chunks_exact_mut(channels) {
                frame.fill(T::from_sample(dsp.next_sample()));
            }
            dsp.end_buffer();
        },
        stream_error_handler("output", stats.clone()),
        None,
    )?)
}

/// The realtime effects chain and the state it needs, independent of the output sample type.
struct Dsp {
    params: Arc<Params>,
    stats: Arc<Stats>,
    consumer: HeapCons<f32>,
    viz: HeapProd<(f32, f32)>,
    target: usize,
    max: usize,
    shifter: PitchShifter,
    gate: Gate,
    shelf: LowShelf,
    gain: Smoother,
    mix: Smoother,
    wet: Smoother,
    last_pitch: f32,
    last_window: f32,
    // Per-buffer control snapshot.
    gate_threshold: f32,
    target_gain: f32,
    target_wet: f32,
    target_mix: f32,
}

impl Dsp {
    fn new(
        rate: u32,
        params: Arc<Params>,
        consumer: HeapCons<f32>,
        viz: HeapProd<(f32, f32)>,
        stats: Arc<Stats>,
        target: usize,
        max: usize,
    ) -> Self {
        let p = &params;
        Self {
            shifter: PitchShifter::new(rate, p.get(Knob::Pitch), p.get(Knob::Window)),
            gate: Gate::new(rate),
            shelf: LowShelf::new(rate, WARMTH_SHELF_HZ, p.get(Knob::Warmth)),
            gain: Smoother::new(rate, 20.0, 0.0),
            mix: Smoother::new(rate, 20.0, p.get(Knob::Mix)),
            wet: Smoother::new(rate, 10.0, 1.0),
            last_pitch: f32::NAN,
            last_window: f32::NAN,
            gate_threshold: 0.0,
            target_gain: 0.0,
            target_wet: 1.0,
            target_mix: p.get(Knob::Mix),
            params,
            stats,
            consumer,
            viz,
            target,
            max,
        }
    }

    /// Reads the controls once per buffer (per-sample smoothing hides the steps) and resyncs
    /// if clock drift has let the input queue run away.
    fn begin_buffer(&mut self) {
        let p = &self.params;
        let pitch = p.get(Knob::Pitch);
        if pitch != self.last_pitch {
            self.shifter.set_semitones(pitch);
            self.last_pitch = pitch;
        }
        let window = p.get(Knob::Window);
        if window != self.last_window {
            self.shifter.set_window_ms(window);
            self.last_window = window;
        }
        self.shelf.set_gain_db(p.get(Knob::Warmth));
        let gate_db = p.get(Knob::Gate);
        self.gate_threshold = if gate_db <= GATE_OFF_DB { 0.0 } else { db_to_gain(gate_db) };
        let mute = p.mute.load(Ordering::Relaxed);
        self.target_gain = if mute { 0.0 } else { db_to_gain(p.get(Knob::Gain)) };
        self.target_wet = if p.bypass.load(Ordering::Relaxed) { 0.0 } else { 1.0 };
        self.target_mix = p.get(Knob::Mix);

        let excess = self.consumer.occupied_len().saturating_sub(self.max);
        if excess > 0 {
            self.consumer.skip(excess + (self.max - self.target));
            self.stats.resyncs.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn next_sample(&mut self) -> f32 {
        let x = self.consumer.try_pop().unwrap_or_else(|| {
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            0.0
        });
        let gated = self.gate.process(x, self.gate_threshold);
        let shifted = self.shelf.process(self.shifter.process_sample(gated, self.mix.next(self.target_mix)));
        let w = self.wet.next(self.target_wet);
        let y = ((x + (shifted - x) * w) * self.gain.next(self.target_gain)).clamp(-1.0, 1.0);
        let _ = self.viz.try_push((x, y));
        y
    }

    fn end_buffer(&mut self) {
        self.stats.gate_open.store(self.gate.is_open(), Ordering::Relaxed);
    }
}

/// A `CALLBACK_MS` period when the platform wants one and the device says what it accepts.
fn buffer_size(supported: &SupportedStreamConfig, rate: u32) -> BufferSize {
    match supported.buffer_size() {
        SupportedBufferSize::Range { min, max } if platform::FIXED_CALLBACK_PERIOD && min <= max => {
            BufferSize::Fixed((rate * CALLBACK_MS / 1000).max(1).clamp(*min, *max))
        }
        _ => BufferSize::Default,
    }
}

/// Picks the best supported config at `rate`: the device's native channel count if possible,
/// then the most convenient sample format.
fn pick_config(ranges: &[SupportedStreamConfigRange], channels: u16, rate: u32) -> Option<SupportedStreamConfig> {
    ranges
        .iter()
        .filter_map(|r| r.try_with_sample_rate(rate))
        .min_by_key(|c| (c.channels() != channels, format_rank(c.sample_format())))
}

fn format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        SampleFormat::I16 => 2,
        SampleFormat::F64 => 3,
        SampleFormat::U8 => 4,
        _ => 5,
    }
}

/// The audio stack reports missed IO deadlines (overloads / xruns) as `Xrun`. They're usually a
/// single dropped buffer at startup or when another app grabs the device, so count them instead
/// of printing each one.
fn stream_error_handler(stream: &'static str, stats: Arc<Stats>) -> impl FnMut(cpal::Error) + Send + 'static {
    move |e| match e.kind() {
        ErrorKind::Xrun => {
            stats.overloads.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            if let Ok(mut last) = stats.last_error.lock() {
                *last = Some(format!("{stream} stream: {e}"));
            }
        }
    }
}

fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".into())
}

/// The backend's stable identifier (e.g. the PulseAudio sink name), when it has one and it adds
/// information beyond the display name.
fn device_id(device: &Device) -> Option<String> {
    device
        .id()
        .ok()
        .map(|id| id.id().to_string())
        .filter(|id| !id.is_empty() && *id != device_name(device))
}

fn device_label(device: &Device) -> String {
    match device_id(device) {
        Some(id) => format!("{}  [{id}]", device_name(device)),
        None => device_name(device),
    }
}

/// Matches `query` case-insensitively against each device's name and id, preferring an exact
/// match over a substring match so near-duplicate names pick the right one.
fn find_device(devices: impl Iterator<Item = Device>, query: &str) -> Result<Device> {
    let q = query.to_lowercase();
    let mut labels = Vec::new();
    let mut partial = None;
    for device in devices {
        let keys = [Some(device_name(&device).to_lowercase()), device_id(&device).map(|id| id.to_lowercase())];
        if keys.iter().flatten().any(|k| *k == q) {
            return Ok(device);
        }
        labels.push(device_label(&device));
        if partial.is_none() && keys.iter().flatten().any(|k| k.contains(&q)) {
            partial = Some(device);
        }
    }
    partial.ok_or_else(|| anyhow!("no device matching \"{query}\". Available:\n  {}", labels.join("\n  ")))
}

/// Whether some output device matches `query` (same rules as `-o`).
pub fn has_output_device(host: &Host, query: &str) -> Result<bool> {
    Ok(find_device(host.output_devices()?, query).is_ok())
}

pub fn list_devices(host: &Host) -> Result<()> {
    let describe = |d: &Device, cfg: Option<SupportedStreamConfig>| {
        let cfg = cfg
            .map(|c| format!("  ({} ch, {} Hz, {})", c.channels(), c.sample_rate(), c.sample_format()))
            .unwrap_or_default();
        format!("  {}{cfg}", device_label(d))
    };
    println!("Input devices:");
    for d in host.input_devices()? {
        println!("{}", describe(&d, d.default_input_config().ok()));
    }
    println!("Output devices:");
    for d in host.output_devices()? {
        println!("{}", describe(&d, d.default_output_config().ok()));
    }
    Ok(())
}
