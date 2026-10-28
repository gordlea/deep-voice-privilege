mod analysis;
mod audio;
mod effects;
mod params;
mod pitch;
mod platform;
mod ui;

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use audio::Engine;
use params::{Knob, Params};

/// Pitch-shift your mic into a virtual audio device (BlackHole on macOS, VB-CABLE on Windows,
/// a PulseAudio/PipeWire null sink on Linux) and use that as your microphone in meetings.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Starting pitch shift in semitones; negative is deeper.
    #[arg(short, long, default_value_t = -3.0, allow_hyphen_values = true)]
    semitones: f32,

    /// Input device name or id (substring match). Defaults to the system default mic.
    #[arg(short, long)]
    input: Option<String>,

    /// Output device name or id (substring match). On Linux it is created if missing.
    #[arg(short, long, default_value = platform::DEFAULT_OUTPUT)]
    output: String,

    /// Don't create the virtual output device when it is missing (Linux).
    #[arg(long)]
    no_create_sink: bool,

    /// Starting grain window in ms. Longer is smoother but adds latency and echo.
    #[arg(long, default_value_t = 40.0)]
    window_ms: f32,

    /// Starting output gain multiplier.
    #[arg(short, long, default_value_t = 1.0)]
    gain: f32,

    /// Run without the interactive UI (also used automatically when not in a terminal).
    #[arg(long)]
    headless: bool,

    /// List audio devices and exit.
    #[arg(short, long)]
    list_devices: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut host = cpal::default_host();

    if args.list_devices {
        return audio::list_devices(&host);
    }

    // Dropped last (declared first), after the engine has released its streams.
    let mut created_sink = None;
    if platform::CAN_CREATE_SINK && !args.no_create_sink && !audio::has_output_device(&host, &args.output)? {
        match platform::create_virtual_sink(&args.output) {
            Ok(sink) => {
                eprintln!("Created virtual sink \"{}\"; it is removed again on exit.", sink.name);
                created_sink = Some(sink);
                // Some backends snapshot the device list when the host is created.
                host = cpal::default_host();
            }
            Err(e) => eprintln!("Could not create \"{}\": {e:#}", args.output),
        }
    }

    let params = Arc::new(Params::new());
    params.set(Knob::Pitch, args.semitones);
    params.set(Knob::Window, args.window_ms);
    params.set(Knob::Gain, analysis::to_db(args.gain));

    let engine = Engine::start(&host, args.input.as_deref(), &args.output, params)?;

    let result = if args.headless || !std::io::stdout().is_terminal() {
        run_headless(&engine)
    } else {
        ui::run(engine)
    };
    drop(created_sink);
    result
}

fn run_headless(engine: &Engine) -> Result<()> {
    println!(
        "{} → {:+} semitones → {} @ {} Hz",
        engine.input_name,
        engine.params.get(Knob::Pitch),
        engine.output_name,
        engine.sample_rate
    );
    println!(
        "In your meeting app, choose \"{}\" as your microphone. Ctrl-C to stop.",
        platform::MIC_HINT
    );

    let (stop_tx, stop_rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = stop_tx.send(());
    })?;

    let mut last = [0; 4];
    let mut last_error = None;
    loop {
        match stop_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let now = engine.stats.glitches();
        if now != last {
            eprintln!(
                "glitches: {} dropped, {} underrun samples, {} drift resyncs, {} {}",
                now[0],
                now[1],
                now[2],
                now[3],
                platform::OVERLOAD_LABEL
            );
            last = now;
        }
        let err = engine.stats.last_error.lock().ok().and_then(|e| e.clone());
        if err.is_some() && err != last_error {
            eprintln!("{}", err.as_deref().unwrap_or_default());
            last_error = err;
        }
    }

    println!("\nStopped.");
    Ok(())
}
