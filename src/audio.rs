use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// State shared between the audio capture callback and the UI.
pub struct Capture {
    /// Mono f32 samples in [-1, 1].
    pub samples: Mutex<Vec<f32>>,
    /// Latest RMS level (f32 bits), for the level meter.
    pub level: AtomicU32,
}

impl Capture {
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
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
    pub fn finish(self) -> (Vec<f32>, u32) {
        let stream = self.stream;
        drop(stream);
        let mut samples = self.capture.samples.lock().unwrap();
        (std::mem::take(&mut *samples), self.sample_rate)
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
    let stream = device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                let mut sum_sq = 0.0f32;
                let mut frames = 0usize;
                let mut out = capture.samples.lock().unwrap();
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
            move |err| {
                eprintln!("audio stream error: {err}");
            },
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

/// Names of input devices that can actually stream, for the settings picker.
pub fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::new();
    for d in devices {
        if d.default_input_config().is_err() {
            continue;
        }
        if let Some(name) = device_name(&d) {
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
    }
    names
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
    let mut partial = None;
    let mut available = Vec::new();
    let devices = host
        .input_devices()
        .context("failed to list input devices")?;
    for d in devices {
        let Some(name) = device_name(&d) else {
            continue;
        };
        if name == want {
            return Ok(d);
        }
        if partial.is_none() && name.to_lowercase().contains(&want.to_lowercase()) {
            partial = Some(d);
        }
        available.push(name);
    }
    partial.ok_or_else(|| {
        anyhow!(
            "microphone {want:?} not found (available: {})",
            available.join(", ")
        )
    })
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

#[cfg(test)]
mod tests {
    #[test]
    #[ignore] // needs real audio hardware — run with --ignored
    fn lists_and_opens_input_devices() {
        let names = super::input_device_names();
        for n in &names {
            eprintln!("input device: {n}");
        }
        assert!(!names.is_empty(), "no input devices on this system");
        for n in &names {
            assert!(super::start(Some(n)).is_ok(), "failed to open device {n:?}");
        }
    }
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
