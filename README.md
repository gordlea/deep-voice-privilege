# deep voice privilege

*Finally, a terminal program that fixes the one thing holding you back: your weak and milquetoast voice.*

You have ideas. Good ones. You present them in meetings and watch them evaporate, only to be
repeated twenty minutes later by someone whose voice sounds like a cello being dragged across a
hardwood floor, at which point everyone nods and says "great point, Derek."

That isn't Derek's intellect. That's **deep voice privilege**, and it has been unfairly distributed
since the dawn of puberty. Deeper voices get rated as more competent, more trustworthy, more
leader-like, and more likely to be allowed to finish a sentence. Until now, the only fixes were
"be born differently" or "take up smoking," neither of which has a good upgrade path.

`deep_voice` sits between your microphone and your meeting app and lowers your voice a few
semitones in real time. Your ideas stay the same. Your interruptions-per-meeting go down. Your
quarterly review becomes a formality. Promotions, respect, and a table at that restaurant are not
guaranteed, but your odds are no longer being decided by vocal fold length.

## What it actually does

- Captures your mic, shifts it down (default 3 semitones, adjustable live from -12 to +12), and
  plays it into a virtual audio device that Webex, Zoom, Teams and friends can select as a
  microphone.
- Granular pitch shifting with about 40 ms of latency, so you still sound like you, only
  managerial.
- A terminal UI with live spectrum, scope, levels, detected pitch before and after, and knobs for
  **Warmth** (bass boost, for when "authoritative" needs to become "narrates documentaries"),
  **Mix**, **Gate**, **Gain** and grain **Window**.
- `--headless` mode for people who have already achieved enough confidence to not watch the graphs.
- Runs on macOS, Windows and Linux. Privilege should be portable.

```
cargo run --release                   # mic → deep voice → virtual device, with the UI
cargo run --release -- -s -4          # start 4 semitones deeper (use responsibly)
cargo run --release -- -s 0           # audition your unprivileged baseline
cargo run --release -- --list-devices # names and [ids] for -i / -o
cargo run --release -- --headless     # no UI, just vibes and glitch counters
```

`-i` / `-o` take a device name or id; matching is case-insensitive, exact first, then substring.

## Install

macOS and Linux, via Homebrew:

```sh
brew install gordlea/tap/deep-voice
```

Windows: grab `deep_voice-<version>-x86_64-pc-windows-msvc.zip` from the
[Releases page](https://github.com/gordlea/deep-voice-privilege/releases) and put `deep_voice.exe`
somewhere on your `PATH`.

Or build it yourself with `cargo install --path .` (see [Building](#building)).

## Setup per platform

One-time step: get a virtual audio device, point `deep_voice` at it, and tell your meeting app
that device is your microphone. Your real microphone keeps doing the hard work in the background,
uncredited, like you used to.

### macOS
1. `brew install blackhole-2ch` ([BlackHole](https://existential.audio/blackhole/)).
2. Run `deep_voice`. The output defaults to `BlackHole 2ch`.
3. In the meeting app, pick **BlackHole 2ch** as the microphone.

If it complains about sample rates, open *Audio MIDI Setup* and set the mic and BlackHole to the
same rate (48000 Hz is a solid, baritone choice).

### Windows
1. Install [VB-CABLE](https://vb-audio.com/Cable/).
2. Run `deep_voice`. The output defaults to `CABLE Input`.
3. In the meeting app, pick **CABLE Output (VB-Audio Virtual Cable)** as the microphone.

Use Windows Terminal or another terminal with Unicode support; the legacy console cannot draw the
graphs and will make you look less competent, defeating the purpose. Audio goes through WASAPI
shared mode, which resamples on output, so rate mismatches are rare; if one appears, set the same
default format on both devices under *Sound settings → device Properties → Advanced*.

### Linux
Audio goes through PulseAudio or PipeWire (via pipewire-pulse). `deep_voice` creates a null sink
called `deepvoice` if it is missing and removes it when you quit, so there is usually nothing to do:

1. Run `deep_voice`.
2. In the meeting app, pick **Monitor of DeepVoice** as the microphone.

Prefer to manage your own privilege infrastructure? Pass `--no-create-sink` and create the sink
yourself:

```
pactl load-module module-null-sink sink_name=deepvoice sink_properties=device.description=DeepVoice
```

If the app is killed outright (SIGKILL), the sink lingers until `pactl unload-module
module-null-sink` or your next login. Without a PulseAudio/PipeWire socket the app falls back to
raw ALSA; then `-o` targets ALSA devices (e.g. `-o pipewire`) and routing has to be done in
`pavucontrol` or `qpwgraph`.

## Controls

| key        | action                                                  |
|------------|---------------------------------------------------------|
| ↑/↓, j/k   | select a control                                        |
| ←/→, h/l   | adjust it (hold Shift for fine steps)                   |
| `+` / `-`  | pitch down/up by half a semitone, whatever is selected  |
| `r` / `R`  | reset this control / reset everything to factory humble |
| space      | bypass (hear your old voice; a cautionary tale)         |
| `m`        | mute                                                    |
| `q`, Esc   | quit and return to society                              |

The **Health** panel counts dropped samples, underruns, drift resyncs and OS-level overloads. A
few at startup are normal. A steady stream means another app is fighting over the device, or your
machine is doing something less important than your career.

## Building

- Rust 1.85+ (edition 2024).
- Linux: ALSA headers and `pkg-config` (`libasound2-dev` on Debian/Ubuntu, `alsa-lib` on Arch,
  `alsa-lib-devel` on Fedora). Needed at build time even though PulseAudio is used at runtime.
- Optional native PipeWire backend: `cargo build --features pipewire`, which needs the
  `libpipewire-0.3` headers and `clang` (`libpipewire-0.3-dev`; `pipewire` + `clang` on Arch;
  `pipewire-devel` on Fedora).
- macOS and Windows need no extra libraries. They never did. Typical.

## Frequently anticipated questions

**Will people notice?** At -2 to -4 semitones, no. At -12, you will sound like a rumour about a
man. Start small; privilege is best acquired gradually.

**Does this work for singing?** It works *on* singing. Whether it works *for* singing is between
you and your choir director.

**Can I go up instead?** Yes, positive semitones are supported. We do not know why you would,
but the software does not judge, which already puts it ahead of most meetings.

**Is this a serious product?** It is a seriously working pitch shifter attached to a joke that
got out of hand. Both halves are maintained.
