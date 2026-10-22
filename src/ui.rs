//! Windows are separate GPUIX processes (`ui/`, built as `dictationapp-ui`):
//! one window each, sent a JSON view per line on stdin, answering with a JSON
//! event per line on stdout. A window closes by exiting, so idle means no
//! process and therefore no surface that could take clicks or focus.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

const UI_BINARY: &str = "dictationapp-ui";
/// Where packages put it, off `PATH`.
const PACKAGED: &str = "/usr/lib/dictationapp/dictationapp-ui";

pub struct Window {
    child: Child,
    views: SyncSender<String>,
    /// The last view handed to the writer, so unchanged views aren't resent.
    last: Option<String>,
}

/// `$DICTATIONAPP_UI`, else `dictationapp-ui` beside this executable, else the
/// packaged one. Debug builds fall back to running the sources with bun.
fn command(kind: &str) -> Result<std::process::Command> {
    if let Some(path) = std::env::var_os("DICTATIONAPP_UI") {
        let mut command = std::process::Command::new(path);
        command.arg(kind);
        return Ok(command);
    }
    let exe = std::env::current_exe().context("no executable path")?;
    let installed = [exe.with_file_name(UI_BINARY), PACKAGED.into()];
    if let Some(path) = installed.into_iter().find(|path| path.is_file()) {
        let mut command = std::process::Command::new(path);
        command.arg(kind);
        return Ok(command);
    }
    let sources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/src/main.tsx");
    if cfg!(debug_assertions) && sources.is_file() {
        let mut command = std::process::Command::new("bun");
        command.arg(sources).arg(kind);
        return Ok(command);
    }
    anyhow::bail!("{UI_BINARY} not found next to {}", exe.display())
}

impl Window {
    /// Start a window process. It opens its window on the first view, so it
    /// can be started early to hide its startup time.
    pub fn spawn(
        kind: &str,
        on_event: impl Fn(serde_json::Value) + Send + 'static,
    ) -> Result<Window> {
        let mut child = command(kind)?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("unable to start {UI_BINARY} {kind}"))?;
        let stdout = child.stdout.take().context("no ui stdout")?;
        let stdin = child.stdin.take().context("no ui stdin")?;
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                match serde_json::from_str(&line) {
                    Ok(event) => on_event(event),
                    Err(_) => eprintln!("ui: {line}"),
                }
            }
        });
        // A stalled window must not block the daemon: the writer owns the
        // pipe, and a full queue drops views (each is complete) until it drains.
        let (views, queued) = sync_channel::<String>(4);
        std::thread::spawn(move || write_views(stdin, queued));
        Ok(Window {
            child,
            views,
            last: None,
        })
    }

    /// Send `view` unless it is what the window already has.
    pub fn show(&mut self, view: &impl Serialize) {
        let Ok(line) = serde_json::to_string(view) else {
            return;
        };
        if self.last.as_ref() == Some(&line) {
            return;
        }
        match self.views.try_send(line.clone()) {
            Ok(()) => self.last = Some(line),
            // Retried with the next view.
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
        }
    }

    pub fn is_open(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

fn write_views(mut stdin: ChildStdin, queued: std::sync::mpsc::Receiver<String>) {
    for view in queued {
        if writeln!(stdin, "{view}")
            .and_then(|()| stdin.flush())
            .is_err()
        {
            return;
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
