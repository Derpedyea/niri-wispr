mod api;
mod app;
mod audio;
mod beep;
mod clipboard;
mod config;
mod hotkey;
mod ipc;
mod niri;
mod settings;
mod tray;
mod typer;

use app::DictationView;
use gpui::{App, AppContext, Application, Entity, Global, KeyBinding, actions};
use ipc::Command;
use std::sync::mpsc::channel;

actions!(dictation, [Toggle, Cancel, Quit]);

// Keep the command/audio state alive while no pill or settings window is open.
struct DictationApp {
    _view: Entity<DictationView>,
}
impl Global for DictationApp {}

fn usage() -> ! {
    eprintln!(
        "dictationapp — run with no arguments to start.\n\
         Commands (sent to a running instance):\n  \
         --toggle   start/stop dictation\n  \
         --start    start recording\n  \
         --stop     stop and transcribe\n  \
         --cancel   discard current recording\n  \
         --settings open the settings window\n  \
         --reload   re-read config and apply changes\n  \
         --quit     exit the app\n  \
         --record <out.wav> [secs]\n  \
                    record from the mic and save a WAV (debug: hear what the model hears)"
    );
    std::process::exit(0);
}

/// Headless capture test: `dictationapp --record /tmp/out.wav 5`
fn record_wav(duration: std::time::Duration, out: &str, mic: Option<&str>) -> anyhow::Result<()> {
    let secs = duration.as_secs_f32();
    eprintln!("recording {secs:.1}s — speak now");
    let rec = audio::start(mic)?;
    std::thread::sleep(duration);
    let (samples, rate) = rec.finish()?;
    let peak = samples.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
    eprintln!(
        "captured {:.2}s at {} Hz — peak {peak:.3}, rms {rms:.3}",
        samples.len() as f32 / rate as f32,
        rate
    );
    let wav = audio::encode_wav(&samples, rate)?;
    std::fs::write(out, &wav)?;
    eprintln!("wrote {} bytes to {out}", wav.len());
    Ok(())
}

fn recording_duration(seconds: Option<&str>) -> anyhow::Result<std::time::Duration> {
    let Some(seconds) = seconds else {
        return Ok(std::time::Duration::from_secs(5));
    };
    let seconds: f32 = seconds.parse()?;
    Ok(std::time::Duration::try_from_secs_f32(seconds)?)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(arg) = args.get(1) {
        if arg == "--record" || arg == "record" {
            let Some(out) = args.get(2).map(String::as_str) else {
                eprintln!("usage: dictationapp --record <out.wav> [secs]");
                std::process::exit(2);
            };
            let result = (|| -> anyhow::Result<()> {
                let duration = recording_duration(args.get(3).map(String::as_str))?;
                let config = config::Config::load()?;
                record_wav(duration, out, config.mic.as_deref())
            })();
            if let Err(e) = result {
                eprintln!("record failed: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        let cmd = match arg.as_str() {
            "--toggle" | "toggle" => Command::Toggle,
            "--start" | "start" => Command::Start,
            "--stop" | "stop" => Command::Stop,
            "--cancel" | "cancel" => Command::Cancel,
            "--quit" | "quit" => Command::Quit,
            "--settings" | "settings" => Command::Settings,
            "--reload" | "reload" => Command::Reload,
            _ => usage(),
        };
        if let Err(e) = ipc::send(cmd) {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return;
    }

    let config = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e:#}");
            std::process::exit(1);
        }
    };

    // Command channel shared by the IPC socket and the evdev hotkey listener.
    let (tx, rx) = channel::<Command>();

    let server = match ipc::listen(tx.clone()) {
        Ok(server) => {
            eprintln!("ipc: listening on {}", server.path().display());
            server
        }
        Err(e) => {
            eprintln!("{e:#}");
            eprintln!("is another dictationapp already running?");
            std::process::exit(1);
        }
    };

    // Global hotkey via evdev (needs read access to /dev/input/event*).
    let watcher = match hotkey::parse_key(&config.hotkey) {
        Ok(key) => {
            let mode = if config.mode == "toggle" {
                hotkey::Mode::Toggle
            } else {
                hotkey::Mode::Hold
            };
            match hotkey::Watcher::start(key, mode, tx.clone()) {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!("hotkey: {e:#}");
                    None
                }
            }
        }
        Err(e) => {
            eprintln!("hotkey: {e:#}");
            None
        }
    };

    // Unicode text delivery through the compositor's virtual keyboard protocol.
    let typer = if config.type_text {
        match typer::Typer::new() {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("typer: {e:#} — falling back to clipboard only");
                None
            }
        }
    } else {
        None
    };

    // The icon in the bar's tray (Noctalia, Waybar, …): opens Settings.
    let tray = match tray::Tray::spawn(tx.clone(), tray::hint(&config.hotkey, &config.mode)) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("tray: {e:#}");
            None
        }
    };

    let tx2 = tx.clone();
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("space", Toggle, None),
            KeyBinding::new("enter", Toggle, None),
            KeyBinding::new("escape", Cancel, None),
            KeyBinding::new("ctrl-q", Quit, None),
        ]);

        cx.set_quit_on_last_window_close(false);
        let view =
            cx.new(|cx| DictationView::new(config.clone(), rx, tx2, typer, watcher, tray, cx));
        cx.set_global(DictationApp { _view: view });
    });
    beep::shutdown();
    // Release the single-instance lock only after the app has stopped.
    drop(server);
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_recording_duration_fails_before_opening_the_microphone() {
        assert_eq!(super::recording_duration(None).unwrap().as_secs(), 5);
        assert_eq!(
            super::recording_duration(Some("0.25")).unwrap().as_millis(),
            250
        );
        for invalid in ["bad", "-1", "NaN", "inf"] {
            assert!(super::recording_duration(Some(invalid)).is_err());
        }
    }
}
