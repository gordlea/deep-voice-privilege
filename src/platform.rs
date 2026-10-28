//! Everything that differs between macOS, Windows and Linux: default device names, user-facing
//! hints, and (Linux only) creating the virtual sink that stands in for BlackHole / VB-CABLE.

use anyhow::Result;

/// Default `--output` device (substring match against device name or id).
#[cfg(target_os = "macos")]
pub const DEFAULT_OUTPUT: &str = "BlackHole 2ch";
#[cfg(target_os = "windows")]
pub const DEFAULT_OUTPUT: &str = "CABLE Input";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const DEFAULT_OUTPUT: &str = "deepvoice";

/// What to pick as the microphone in the meeting app.
#[cfg(target_os = "macos")]
pub const MIC_HINT: &str = "BlackHole 2ch";
#[cfg(target_os = "windows")]
pub const MIC_HINT: &str = "CABLE Output (VB-Audio Virtual Cable)";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const MIC_HINT: &str = "Monitor of DeepVoice";

/// How to get a virtual audio device when none is found.
#[cfg(target_os = "macos")]
pub const SETUP_HINT: &str = "Install BlackHole (brew install blackhole-2ch) and pass its name with -o.";
#[cfg(target_os = "windows")]
pub const SETUP_HINT: &str =
    "Install VB-CABLE (https://vb-audio.com/Cable/) and pass the \"CABLE Input\" device with -o.";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const SETUP_HINT: &str = "Create a virtual sink with:\n  pactl load-module module-null-sink \
    sink_name=deepvoice sink_properties=device.description=DeepVoice\nthen pass its name with -o.";

/// How to fix an input/output sample-rate mismatch.
#[cfg(target_os = "macos")]
pub const RATE_HINT: &str =
    "Open Audio MIDI Setup and set both devices to the same rate (e.g. 48000 Hz).";
#[cfg(target_os = "windows")]
pub const RATE_HINT: &str = "In Sound settings open each device's Properties > Advanced and set both \
    to the same default format (e.g. 48000 Hz).";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const RATE_HINT: &str = "Set both devices to the same rate, e.g. default.clock.rate = 48000 in \
    ~/.config/pipewire/pipewire.conf.d/, or run through PulseAudio/PipeWire instead of raw ALSA.";

/// Label for missed audio deadlines reported by the OS audio stack.
#[cfg(target_os = "macos")]
pub const OVERLOAD_LABEL: &str = "CoreAudio overloads";
#[cfg(target_os = "windows")]
pub const OVERLOAD_LABEL: &str = "WASAPI glitches";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const OVERLOAD_LABEL: &str = "xruns";

/// Whether `create_virtual_sink` can work here (Linux with `pactl`).
pub const CAN_CREATE_SINK: bool = cfg!(target_os = "linux");

/// Whether to ask for a fixed, short callback period. PulseAudio otherwise delivers audio in
/// large bursts; WASAPI and PipeWire treat it as a latency hint. CoreAudio applies it to the device
/// globally (shared with other apps), so macOS keeps the device's own setting.
pub const FIXED_CALLBACK_PERIOD: bool = !cfg!(target_os = "macos");

/// A virtual sink this process created; unloaded again on drop.
pub struct VirtualSink {
    #[allow(dead_code)]
    module_id: String,
    pub name: String,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::VirtualSink;
    use anyhow::{Context, Result, bail};
    use std::process::Command;

    /// Human-readable name given to the auto-created sink (what the meeting app shows).
    const SINK_DESCRIPTION: &str = "DeepVoice";

    /// Creates a null sink called `name` via `pactl` (works on PulseAudio and PipeWire).
    pub fn create_virtual_sink(name: &str) -> Result<VirtualSink> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
            bail!("\"{name}\" is not a valid sink name (use letters, digits, '.', '_' or '-')");
        }
        let description = if name == super::DEFAULT_OUTPUT { SINK_DESCRIPTION } else { name };
        let output = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
                &format!("sink_properties=device.description={description}"),
            ])
            .output()
            .context("could not run pactl")?;
        if !output.status.success() {
            bail!("pactl load-module failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        let module_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if module_id.is_empty() || !module_id.chars().all(|c| c.is_ascii_digit()) {
            bail!("unexpected pactl output: {module_id:?}");
        }
        Ok(VirtualSink { module_id, name: name.to_string() })
    }

    impl Drop for VirtualSink {
        fn drop(&mut self) {
            let _ = Command::new("pactl").args(["unload-module", &self.module_id]).status();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::VirtualSink;
    use anyhow::{Result, bail};

    pub fn create_virtual_sink(name: &str) -> Result<VirtualSink> {
        bail!("cannot create \"{name}\" automatically on this platform. {}", super::SETUP_HINT)
    }
}

/// Creates the virtual output device if this platform knows how (Linux via `pactl`).
pub fn create_virtual_sink(name: &str) -> Result<VirtualSink> {
    imp::create_virtual_sink(name)
}
