use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// State shared between the audio capture callback and the UI.
pub struct Capture {
    /// Mono f32 samples in [-1, 1].
    pub samples: Mutex<Vec<f32>>,
    /// Latest RMS level (f32 bits), for the level meter.
    pub level: AtomicU32,
    /// A stream error permanently invalidates this recording, even if capture resumes.
    failure: OnceLock<String>,
}

impl Capture {
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Read without clearing the error so the UI and finish-time check both see it.
    pub fn failure(&self) -> Option<String> {
        self.failure.get().cloned()
    }

    fn fail(&self, error: String) {
        self.failure.get_or_init(|| error);
        self.level.store(0, Ordering::Relaxed);
    }

    /// cpal keeps streaming after an overrun or a reroute, so those cost a few
    /// milliseconds of audio, not the recording. Everything else fails closed.
    fn stream_error(&self, error: cpal::Error) {
        eprintln!("audio stream error: {error}");
        if !matches!(
            error.kind(),
            cpal::ErrorKind::Xrun
                | cpal::ErrorKind::DeviceChanged
                | cpal::ErrorKind::RealtimeDenied
        ) {
            self.fail(error.to_string());
        }
    }

    /// Called only after the stream has stopped and its callbacks have joined.
    fn take_samples(&self) -> Result<Vec<f32>> {
        if let Some(error) = self.failure() {
            return Err(anyhow!("audio capture failed: {error}"));
        }
        let mut samples = self
            .samples
            .lock()
            .map_err(|_| anyhow!("audio capture samples were poisoned"))?;
        Ok(std::mem::take(&mut *samples))
    }
}

/// An in-progress recording. Keeps the cpal stream alive until finished.
pub struct Recording {
    stream: Stream,
    pub capture: Arc<Capture>,
    pub sample_rate: u32,
}

impl Recording {
    /// Stop capturing and return (mono samples, sample_rate).
    pub fn finish(self) -> Result<(Vec<f32>, u32)> {
        let stream = self.stream;
        drop(stream);
        // A callback may fail while the stream is shutting down. Check afterwards.
        Ok((self.capture.take_samples()?, self.sample_rate))
    }
}

fn build_stream<T: SizedSample>(
    device: &Device,
    config: StreamConfig,
    capture: Arc<Capture>,
) -> Result<Stream>
where
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let error_capture = capture.clone();
    let stream = device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                if capture.failure.get().is_some() {
                    return;
                }
                let mut sum_sq = 0.0f32;
                let mut frames = 0usize;
                let Ok(mut out) = capture.samples.lock() else {
                    capture.fail("audio capture samples were poisoned".into());
                    return;
                };
                for frame in data.chunks(channels) {
                    let mono: f32 =
                        frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / channels as f32;
                    sum_sq += mono * mono;
                    frames += 1;
                    out.push(mono);
                }
                if frames > 0 {
                    let rms = (sum_sq / frames as f32).sqrt();
                    // Boost a bit for visual feedback; speech RMS is typically low.
                    let boosted = (rms * 4.0).min(1.0);
                    capture.level.store(boosted.to_bits(), Ordering::Relaxed);
                }
            },
            move |err| error_capture.stream_error(err),
            None,
        )
        .context("failed to build input stream")?;
    Ok(stream)
}

fn device_name(device: &Device) -> Option<String> {
    device
        .description()
        .ok()
        .map(|d| d.name().to_string())
        .filter(|n| !n.trim().is_empty())
}

/// Input device names: `all` as recording resolves them, `cards` for the
/// settings picker. ALSA also lists its plugins ("Rate Converter Plugin…",
/// "Discard all samples…") — not microphones; "System default" already
/// covers the sound server. Not test-opened: a mic the sound server is
/// briefly holding would vanish.
pub struct InputDevices {
    pub all: Vec<String>,
    pub cards: Vec<String>,
}

pub fn input_devices() -> InputDevices {
    let mut found = InputDevices {
        all: Vec::new(),
        cards: Vec::new(),
    };
    let Ok(devices) = cpal::default_host().input_devices() else {
        return found;
    };
    for d in devices {
        let Some(name) = device_name(&d) else {
            continue;
        };
        if d.id().is_ok_and(|id| is_card_input(id.id())) && !found.cards.contains(&name) {
            found.cards.push(name.clone());
        }
        if !found.all.contains(&name) {
            found.all.push(name);
        }
    }
    found
}

/// Which of `names` a configured `mic` means: the exact name, else the first
/// containing it, ignoring case. Recording and the settings picker both use
/// it, so they agree on which device a setting like "USB" picks.
pub fn match_mic(want: &str, names: &[String]) -> Option<usize> {
    let want = want.trim();
    if want.is_empty() {
        return None;
    }
    names.iter().position(|name| name == want).or_else(|| {
        let want = want.to_lowercase();
        names
            .iter()
            .position(|name| name.to_lowercase().contains(&want))
    })
}

/// A card's capture PCMs: `sysdefault:CARD=…`, `front:CARD=…`, `hw:CARD=…`,
/// `plughw:CARD=…`. Plugins and sound servers have bare names (`lavrate`,
/// `pipewire`); `usbstream:` and other per-card PCMs can't record.
fn is_card_input(alsa_id: &str) -> bool {
    ["sysdefault:", "front:", "hw:", "plughw:"]
        .iter()
        .any(|prefix| alsa_id.starts_with(prefix))
}

/// Pick the input device for `mic` (None/blank = system default). A configured
/// name matches exactly, else case-insensitive substring.
fn pick_input_device(mic: Option<&str>) -> Result<Device> {
    let host = cpal::default_host();
    let Some(want) = mic.map(str::trim).filter(|m| !m.is_empty()) else {
        return host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device found"));
    };
    let mut devices: Vec<(String, Device)> = host
        .input_devices()
        .context("failed to list input devices")?
        .filter_map(|d| Some((device_name(&d)?, d)))
        .collect();
    let names: Vec<String> = devices.iter().map(|(name, _)| name.clone()).collect();
    match match_mic(want, &names) {
        Some(index) => Ok(devices.swap_remove(index).1),
        None => Err(anyhow!(
            "microphone {want:?} not found (available: {})",
            names.join(", ")
        )),
    }
}

/// Start capturing from the configured input device (`mic` = device name,
/// None/blank = system default).
pub fn start(mic: Option<&str>) -> Result<Recording> {
    let device = pick_input_device(mic)?;
    let supported = device
        .default_input_config()
        .context("failed to get default input config")?;
    let config = supported.config();
    let sample_rate = config.sample_rate;
    eprintln!(
        "audio input: {} ({} ch, {} Hz, {})",
        device_name(&device).unwrap_or_else(|| "?".into()),
        config.channels,
        sample_rate,
        supported.sample_format(),
    );

    let capture = Arc::new(Capture {
        samples: Mutex::new(Vec::with_capacity(
            sample_rate as usize * 30, // ~30s of mono audio
        )),
        level: AtomicU32::new(0),
        failure: OnceLock::new(),
    });

    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(&device, config, capture.clone()),
        SampleFormat::F64 => build_stream::<f64>(&device, config, capture.clone()),
        SampleFormat::I8 => build_stream::<i8>(&device, config, capture.clone()),
        SampleFormat::I16 => build_stream::<i16>(&device, config, capture.clone()),
        SampleFormat::I24 => build_stream::<cpal::I24>(&device, config, capture.clone()),
        SampleFormat::I32 => build_stream::<i32>(&device, config, capture.clone()),
        SampleFormat::I64 => build_stream::<i64>(&device, config, capture.clone()),
        SampleFormat::U8 => build_stream::<u8>(&device, config, capture.clone()),
        SampleFormat::U16 => build_stream::<u16>(&device, config, capture.clone()),
        SampleFormat::U24 => build_stream::<cpal::U24>(&device, config, capture.clone()),
        SampleFormat::U32 => build_stream::<u32>(&device, config, capture.clone()),
        SampleFormat::U64 => build_stream::<u64>(&device, config, capture.clone()),
        other => Err(anyhow!("unsupported input sample format: {other}")),
    }?;

    stream.play().context("failed to start input stream")?;
    Ok(Recording {
        stream,
        capture,
        sample_rate,
    })
}

/// Encode mono f32 samples as 16-bit PCM WAV bytes.
pub fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer =
            hound::WavWriter::new(&mut cursor, spec).context("failed to create wav writer")?;
        for &s in samples {
            let clamped = s.clamp(-1.0, 1.0);
            let v = (clamped * i16::MAX as f32) as i16;
            writer
                .write_sample(v)
                .context("failed to write wav sample")?;
        }
        writer.finalize().context("failed to finalize wav")?;
    }
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(samples: Vec<f32>) -> Capture {
        Capture {
            samples: Mutex::new(samples),
            level: AtomicU32::new(0),
            failure: OnceLock::new(),
        }
    }

    #[test]
    fn failed_capture_never_returns_a_partial_recording() {
        let capture = capture(vec![0.1; 100]);
        assert!(capture.failure().is_none());
        capture.fail("microphone disconnected".into());
        assert_eq!(
            capture.failure().as_deref(),
            Some("microphone disconnected")
        );
        assert!(capture.take_samples().is_err());
        assert!(capture.take_samples().is_err());
        // A later callback cannot hide the first failure or mark the prefix complete.
        capture.fail("later error".into());
        assert_eq!(
            capture.failure().as_deref(),
            Some("microphone disconnected")
        );
    }

    #[test]
    fn only_stream_errors_that_stop_capture_fail_it() {
        let capture = capture(vec![0.1]);
        for kind in [cpal::ErrorKind::Xrun, cpal::ErrorKind::DeviceChanged] {
            capture.stream_error(cpal::Error::with_message(kind, "glitch"));
        }
        assert!(capture.failure().is_none());
        capture.stream_error(cpal::Error::with_message(
            cpal::ErrorKind::DeviceNotAvailable,
            "unplugged",
        ));
        assert!(capture.take_samples().is_err());
    }

    #[test]
    fn a_new_capture_after_failure_can_complete() {
        let failed = capture(vec![0.1]);
        failed.fail("disconnected".into());
        drop(failed);

        let retry = capture(vec![0.2, -0.2]);
        assert_eq!(retry.take_samples().unwrap(), vec![0.2, -0.2]);
        assert!(retry.samples.lock().unwrap().is_empty());
    }

    #[test]
    fn configured_mic_prefers_an_exact_name_then_a_substring() {
        let names = ["USB Microphone, USB Audio", "USB", "Webcam Mic"].map(String::from);
        assert_eq!(match_mic("USB", &names), Some(1));
        assert_eq!(match_mic(" usb micro ", &names), Some(0));
        assert_eq!(match_mic("webcam", &names), Some(2));
        assert_eq!(match_mic("Studio", &names), None);
        assert_eq!(match_mic("  ", &names), None);
    }

    #[test]
    #[ignore] // needs real audio hardware — run with --ignored
    fn lists_and_opens_input_devices() {
        let names = super::input_devices().cards;
        for n in &names {
            eprintln!("input device: {n}");
        }
        assert!(!names.is_empty(), "no input devices on this system");
        for n in &names {
            assert!(super::start(Some(n)).is_ok(), "failed to open device {n:?}");
        }
    }
}
