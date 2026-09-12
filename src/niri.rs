//! niri compositor integration.
//! niri 26.04 ignores `default-floating-position` on this setup, so the app
//! positions the pill itself via `niri msg` after the window opens.

use anyhow::{Context, Result};
use std::process::Command;
use std::time::Duration;

const PILL_W: f64 = 320.0;
const PILL_H: f64 = 64.0;
/// Gap between the bottom of the working area and the pill.
const BOTTOM_GAP: f64 = 80.0;

/// If niri is the running compositor, move our window to the bottom-center
/// of whatever output it opened on. Safe no-op elsewhere.
pub fn place_at_bottom() {
    if Command::new("niri")
        .args(["msg", "--version"])
        .output()
        .is_err()
    {
        return;
    }
    std::thread::spawn(|| {
        // Give niri a moment to map the window.
        std::thread::sleep(Duration::from_millis(400));
        if let Err(e) = place() {
            eprintln!("niri: could not position pill: {e:#}");
        }
    });
}

fn place() -> Result<()> {
    let windows = msg_json(&["windows"])?;
    let win = windows
        .as_array()
        .and_then(|w| {
            w.iter()
                .find(|w| w["app_id"].as_str() == Some("dictationapp"))
        })
        .context("dictationapp window not found")?;
    let wid = win["id"].as_u64().context("no window id")?;
    let ws_id = win["workspace_id"].as_u64().context("no workspace id")?;

    // Which output is this window's workspace on?
    let workspaces = msg_json(&["workspaces"])?;
    let output_name = workspaces
        .as_array()
        .and_then(|w| w.iter().find(|w| w["id"].as_u64() == Some(ws_id)))
        .and_then(|w| w["output"].as_str().map(str::to_owned))
        .context("workspace not found")?;

    // That output's logical size.
    let outputs = msg_json(&["outputs"])?;
    let logical = outputs
        .as_object()
        .and_then(|o| o.get(&output_name))
        .and_then(|o| o["logical"].as_object().cloned())
        .context("output not found")?;
    let w = logical["width"].as_f64().context("no width")?;
    let h = logical["height"].as_f64().context("no height")?;

    let x = ((w - PILL_W) / 2.0).round() as i64;
    let y = (h - PILL_H - BOTTOM_GAP).round() as i64;

    let out = Command::new("niri")
        .args([
            "msg",
            "action",
            "move-floating-window",
            "--id",
            &wid.to_string(),
            "-x",
            &x.to_string(),
            "-y",
            &y.to_string(),
        ])
        .output()
        .context("move-floating-window failed")?;
    if !out.status.success() {
        anyhow::bail!("niri msg: {}", String::from_utf8_lossy(&out.stderr));
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
    Ok(serde_json::from_slice(&out.stdout).context("bad niri json")?)
}
