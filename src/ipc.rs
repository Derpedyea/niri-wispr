use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle,
    Start,
    Stop,
    Cancel,
    Quit,
    /// Open the settings window.
    Settings,
    /// Re-read config.toml and apply changes (restarts the hotkey watcher if needed).
    Reload,
}

impl Command {
    fn parse(s: &str) -> Option<Command> {
        match s.trim() {
            "toggle" => Some(Command::Toggle),
            "start" => Some(Command::Start),
            "stop" => Some(Command::Stop),
            "cancel" => Some(Command::Cancel),
            "quit" => Some(Command::Quit),
            "settings" => Some(Command::Settings),
            "reload" => Some(Command::Reload),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Command::Toggle => "toggle",
            Command::Start => "start",
            Command::Stop => "stop",
            Command::Cancel => "cancel",
            Command::Quit => "quit",
            Command::Settings => "settings",
            Command::Reload => "reload",
        }
    }
}

pub fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!(
        "dictationapp-{}.sock",
        std::env::var("USER").unwrap_or_else(|_| "unknown".into())
    ))
}

/// Send a command to the running instance. Used by CLI subcommands.
pub fn send(cmd: Command) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path())
        .context("could not reach dictationapp — is it running?")?;
    stream.write_all(cmd.as_str().as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}

/// Bind the socket and forward parsed commands to `tx`.
/// Takes over the socket file if a previous instance left it behind.
/// Fails only if a live instance is already listening.
pub fn listen(tx: Sender<Command>) -> Result<PathBuf> {
    let path = socket_path();
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Stale socket file? If nothing answers a connect, take it over.
            if UnixStream::connect(&path).is_ok() {
                return Err(anyhow::anyhow!("another dictationapp is already running"));
            }
            std::fs::remove_file(&path).ok();
            UnixListener::bind(&path)
                .with_context(|| format!("failed to bind {}", path.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("failed to bind {}", path.display())),
    };
    let bound = path.clone();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let tx = tx.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if let Some(cmd) = Command::parse(&line) {
                        match tx.send(cmd) {
                            Ok(()) => eprintln!("ipc: sent {cmd:?}"),
                            Err(e) => eprintln!("ipc: send failed: {e}"),
                        }
                    }
                }
            });
        }
    });

    Ok(bound)
}
