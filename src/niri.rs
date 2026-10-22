//! niri compositor integration. The pill is a layer-shell surface the
//! compositor places itself; only Settings, a normal window, needs help.

use anyhow::{Context, Result};
use std::process::Command;

/// Bring an open window forward (switching workspace if needed). A second
/// launch has no input serial to activate with, so niri would ignore it.
pub fn focus_window(pid: u32, title: &'static str) {
    std::thread::spawn(move || {
        if let Err(e) = focus(pid, title) {
            eprintln!("niri: could not focus {title:?}: {e:#}");
        }
    });
}

/// The pid owning the focused window, if niri runs and something is focused.
pub fn focused_pid() -> Option<u32> {
    let window = msg_json(&["focused-window"]).ok()?;
    window["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
}

fn focus(pid: u32, title: &str) -> Result<()> {
    let windows = msg_json(&["windows"])?;
    let id = windows
        .as_array()
        .and_then(|w| {
            w.iter().find(|w| {
                w["pid"].as_u64() == Some(u64::from(pid)) && w["title"].as_str() == Some(title)
            })
        })
        .and_then(|w| w["id"].as_u64())
        .with_context(|| format!("no window titled {title:?}"))?;
    run_action(&["focus-window", "--id", &id.to_string()])
}

fn run_action(args: &[&str]) -> Result<()> {
    let mut full = vec!["msg", "action"];
    full.extend_from_slice(args);
    let out = Command::new("niri")
        .args(&full)
        .output()
        .context("niri msg action failed")?;
    if !out.status.success() {
        anyhow::bail!(
            "niri {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn msg_json(args: &[&str]) -> Result<serde_json::Value> {
    let mut full = vec!["msg", "--json"];
    full.extend_from_slice(args);
    let out = Command::new("niri")
        .args(&full)
        .output()
        .context("niri msg failed")?;
    if !out.status.success() {
        anyhow::bail!("niri msg: {}", String::from_utf8_lossy(&out.stderr));
    }
    serde_json::from_slice(&out.stdout).context("bad niri json")
}
