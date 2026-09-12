//! Subtle audio cues for recording start/stop, like Wispr Flow.
//! Generates tiny WAVs in the cache dir once, then plays them via a
//! PipeWire/PulseAudio player (fire-and-forget, no audio lib needed).

use std::f32::consts::TAU;
use std::path::PathBuf;
use std::process::Command;

const RATE: u32 = 48_000;

#[derive(Clone, Copy)]
enum Cue {
    /// Rising two-tone "listening now".
    Start,
    /// Falling tone "done".
    Stop,
    /// Low blip on errors.
    Error,
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("dictationapp")
}

fn cue_path(cue: Cue) -> PathBuf {
    cache_dir().join(match cue {
        Cue::Start => "start.wav",
        Cue::Stop => "stop.wav",
        Cue::Error => "error.wav",
    })
}

/// (frequency Hz, duration ms) segments per cue.
fn segments(cue: Cue) -> &'static [(f32, u32)] {
    match cue {
        Cue::Start => &[(660.0, 60), (990.0, 90)],
        Cue::Stop => &[(880.0, 60), (550.0, 90)],
        Cue::Error => &[(220.0, 180)],
    }
}

fn ensure_wav(cue: Cue) -> Option<PathBuf> {
    let path = cue_path(cue);
    if path.exists() {
        return Some(path);
    }
    std::fs::create_dir_all(path.parent()?).ok()?;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).ok()?;
    for &(freq, ms) in segments(cue) {
        let n = (RATE as u64 * ms as u64 / 1000) as u32;
        for i in 0..n {
            // Raised-cosine envelope on the outer 12ms to avoid clicks.
            let fade = (RATE as f32 * 0.012).min(n as f32 / 2.0);
            let amp = if i as f32 > (n as f32 - fade) {
                (n as f32 - i as f32) / fade
            } else {
                (i as f32 / fade).min(1.0)
            };
            let s = (i as f32 * freq * TAU / RATE as f32).sin() * amp * 0.25;
            w.write_sample((s * i16::MAX as f32) as i16).ok()?;
        }
    }
    w.finalize().ok()?;
    Some(path)
}

fn play_file(path: &std::path::Path) {
    let s = path.to_string_lossy().into_owned();
    for (bin, args) in [
        ("pw-play", vec![s.clone()]),
        ("paplay", vec![s.clone()]),
        ("aplay", vec!["-q".into(), s.clone()]),
    ] {
        if Command::new("sh")
            .args(["-c", &format!("command -v {bin} >/dev/null")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            let _ = Command::new(bin).args(&args).spawn();
            return;
        }
    }
}

pub fn start() {
    play(Cue::Start);
}
pub fn stop() {
    play(Cue::Stop);
}
pub fn error() {
    play(Cue::Error);
}

fn play(cue: Cue) {
    if let Some(path) = ensure_wav(cue) {
        play_file(&path);
    }
}
