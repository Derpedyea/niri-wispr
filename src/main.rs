mod api;
mod app;
mod audio;
mod beep;
mod config;
mod hotkey;
mod ipc;
mod niri;
mod settings;
mod typer;

use app::DictationView;
use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, actions, px, size,
};
use ipc::Command;
use std::sync::mpsc::channel;

actions!(dictation, [Toggle, Cancel, Quit]);

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
         --record <secs> <out.wav>\n  \
                    record from the mic and save a WAV (debug: hear what the model hears)"
    );
    std::process::exit(0);
}

/// Headless capture test: `dictationapp --record 5 /tmp/out.wav`
fn record_wav(secs: f32, out: &str, mic: Option<&str>) -> anyhow::Result<()> {
    eprintln!("recording {secs:.1}s — speak now");
    let rec = audio::start(mic)?;
    std::thread::sleep(std::time::Duration::from_secs_f32(secs));
    let (samples, rate) = rec.finish();
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(arg) = args.get(1) {
        if arg == "--record" || arg == "record" {
            let secs: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5.0);
            let Some(out) = args.get(2).map(String::as_str) else {
                eprintln!("usage: dictationapp --record <out.wav> [secs]");
                std::process::exit(2);
            };
            let mic = config::Config::load().ok().and_then(|c| c.mic);
            if let Err(e) = record_wav(secs, out, mic.as_deref()) {
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

    match ipc::listen(tx.clone()) {
        Ok(path) => eprintln!("ipc: listening on {}", path.display()),
        Err(e) => {
            eprintln!("{e:#}");
            eprintln!("is another dictationapp already running?");
            std::process::exit(1);
        }
    }

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

    // Virtual keyboard for typing into the focused window.
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

    let tx2 = tx.clone();
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("space", Toggle, None),
            KeyBinding::new("enter", Toggle, None),
            KeyBinding::new("escape", Cancel, None),
            KeyBinding::new("ctrl-q", Quit, None),
        ]);

        let pill = size(px(300.0), px(64.0));
        let bounds = Bounds::centered(None, pill, cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Dictation".into()),
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                window_background: WindowBackgroundAppearance::Transparent,
                app_id: Some("dictationapp".to_string()),
                focus: false,
                is_resizable: false,
                window_min_size: Some(pill),
                ..Default::default()
            },
            move |window, cx| {
                let view =
                    cx.new(|cx| DictationView::new(config.clone(), rx, tx2, typer, watcher, cx));
                window.set_window_title("Dictation");
                window.resize(pill);
                view
            },
        )
        .unwrap();
        niri::place_at_bottom();
        cx.activate(true);
    });
}
