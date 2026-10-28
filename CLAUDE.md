# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`deep_voice` is a single-binary Rust CLI/TUI that pitch-shifts the microphone in real time into a
virtual audio device (BlackHole on macOS, VB-CABLE on Windows, an auto-created PulseAudio/PipeWire
null sink on Linux) so meeting apps can use the shifted voice as their mic. Audio I/O is `cpal`
0.18, the UI is `ratatui`. README.md has the per-OS user setup.

## Commands

```sh
cargo build                       # debug build (Linux needs ALSA headers + pkg-config)
cargo run -- --list-devices       # names and [ids] usable with -i / -o
cargo run -- --headless           # no TUI; prints glitch counters every 5 s
cargo run -- -s -4 -o <device>    # TUI, start 4 semitones down
cargo test                        # DSP/UI unit tests (no audio hardware needed)
cargo test three_semitones_down   # single test by name substring
cargo clippy
cargo check --features pipewire   # optional native PipeWire backend (needs libpipewire headers + clang)
cargo check --target x86_64-pc-windows-gnu    # cross-check the Windows cfg branches
cargo check --target aarch64-apple-darwin     # cross-check the macOS cfg branches
```

There is no rustfmt config and lines run to ~110 columns; don't reformat whole files.

Manual Linux audio check (PipeWire/Pulse): run the app, confirm with `pactl list short sinks`
that `deepvoice` is RUNNING and `pactl list short sink-inputs` shows the stream, then feed a tone
by playing a wav into a second null sink and passing its monitor as `-i`, and record
`deepvoice.monitor` with `parecord`. The sink must be gone after quitting.

## Architecture

Two threads of execution that must never block each other:

- **Audio callbacks** (`src/audio.rs`). `Engine::start` opens an input and an output stream. The
  input callback downmixes to mono f32 and pushes into a lock-free `ringbuf` (`HeapRb`) prefilled
  to a 20 ms target. The output callback (`Dsp`) pops from that ring, runs gate → granular pitch
  shifter → low shelf → mix/bypass/gain, fans the result to every output channel, and pushes
  `(raw, processed)` pairs into a second ring for the UI. If the ring exceeds 80 ms (clock drift
  between devices) it skips back to the target and counts a resync. Streams are built generically
  over the sample type (`build_input<T>` / `build_output<T>`) and dispatched on `SampleFormat`.
- **UI thread** (`src/ui.rs`, `src/analysis.rs`). A 33 ms frame loop drains the viz ring, keeps
  FFT-sized histories, computes spectrum bands, RMS and pitch (autocorrelation) and draws controls,
  levels, health counters, spectrum and scope. `ui::run` owns the `Engine`; `main.rs` runs
  `run_headless` instead when `--headless` or stdout isn't a terminal.

Shared state is all atomics (`src/params.rs`): `Params` holds one `AtomicF32` per `Knob` plus
`bypass`/`mute` flags; `Stats` holds glitch counters. The only `Mutex` is `Stats::last_error`,
touched only from cpal error callbacks, never the data path. Nothing in a data callback may
allocate, lock or print. Control values are read once per buffer in `Dsp::begin_buffer` and
smoothed per sample (`effects::Smoother`), so UI changes never click.

**Adding a control**: add a `Knob` variant, its entry in `Knob::ALL`, `spec()` and `format()`, a
slot in `Params`, and consume it in `Dsp::begin_buffer`. The controls panel, keys and reset
logic iterate `Knob::ALL`, so they need no change.

**DSP** (`src/pitch.rs`, `src/effects.rs`) is pure per-sample code with no cpal dependency; it is
where the unit tests live (synthetic sines, checking frequency ratios and boundedness).

### Platform handling

`src/platform.rs` is the only file allowed to use `cfg(target_os)`. Everything OS-specific is a
constant there (default output device, mic hint, setup/rate hints, overload label,
`FIXED_CALLBACK_PERIOD`, `CAN_CREATE_SINK`) plus `create_virtual_sink`, which on Linux shells out
to `pactl load-module module-null-sink` and unloads it in `Drop`. `main.rs` holds that guard and
drops it after the engine so the stream is closed before the sink disappears.

Backend facts that shaped the code (don't undo them without re-testing):

- Linux uses cpal's pure-Rust `pulseaudio` feature (enabled via a `[target.'cfg(target_os =
  "linux")']` dependency), which sees Pulse/PipeWire sinks by name. Plain ALSA can't target a sink
  and only appears as a fallback when no Pulse socket exists.
- The Pulse backend delivers audio in large bursts under `BufferSize::Default`, producing massive
  drop/underrun counts. `audio::buffer_size` therefore requests a 10 ms `BufferSize::Fixed`,
  clamped to the device's advertised range, on every platform except macOS, where a fixed size
  changes the CoreAudio device setting globally.
- WASAPI output auto-converts sample rates; capture does not. Hence the engine runs at the
  input's native rate and only falls back to the output's rate if the input also supports it.
- Device lookup (`find_device`) matches the query case-insensitively against both the display name
  and the backend id (`DeviceId::id()`, e.g. the Pulse sink name), exact match before substring,
  because ALSA and Pulse lists contain near-duplicate names.

### UI conventions

A `tui-design` skill is installed under `.claude/skills/`; use it for layout or styling work.
`Theme::detect` honours `NO_COLOR`; layouts switch at 80 columns (controls+visuals side by side
vs stacked without the scope) and refuse to draw below 40x16. `KeyEventKind::Release` is filtered
so Windows doesn't double-fire keys.
