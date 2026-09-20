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
/// of the focused output. Safe no-op elsewhere.
pub fn place_at_bottom() {
    // Give niri a moment to map the window.
    spawn_place(Duration::from_millis(400));
}

/// Follow the focused output when a recording reuses an already visible pill.
pub fn reposition() {
    spawn_place(Duration::from_millis(50));
}

fn spawn_place(delay: Duration) {
    if Command::new("niri")
        .args(["msg", "--version"])
        .output()
        .is_err()
    {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(delay);
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
            w.iter().find(|w| {
                w["app_id"].as_str() == Some("dictationapp")
                    && w["title"].as_str() == Some("Dictation")
            })
        })
        .context("dictationapp window not found")?;
    let wid = win["id"].as_u64().context("no window id")?;
    let ws_id = win["workspace_id"].as_u64().context("no workspace id")?;

    // The pill belongs on the focused workspace — the output the user is
    // looking at. Fall back to its current workspace if none is focused.
    let workspaces = msg_json(&["workspaces"])?;
    let ws_list = workspaces.as_array().context("bad workspaces json")?;
    let own_ws = ws_list.iter().find(|w| w["id"].as_u64() == Some(ws_id));
    let target = ws_list
        .iter()
        .find(|w| w["is_focused"].as_bool() == Some(true))
        .or(own_ws)
        .context("workspace not found")?;
    let target_ws = target["id"].as_u64().context("no workspace id")?;
    let target_idx = target["idx"].as_u64().context("no workspace index")?;
    let output_name = target["output"]
        .as_str()
        .context("workspace has no output")?
        .to_owned();
    let own_output = own_ws.and_then(|w| w["output"].as_str()).unwrap_or("");

    if target_ws != ws_id {
        if own_output != output_name {
            // Cross-output: lands on that output's active workspace, which is
            // the focused one.
            run_action(&[
                "move-window-to-monitor",
                "--id",
                &wid.to_string(),
                &output_name,
            ])?;
        } else {
            // Same output, inactive workspace: the index resolves on the
            // window's own output — the focused one here.
            run_action(&[
                "move-window-to-workspace",
                "--window-id",
                &wid.to_string(),
                "--focus",
                "false",
                &target_idx.to_string(),
            ])?;
        }
    }

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

    run_action(&[
        "move-floating-window",
        "--id",
        &wid.to_string(),
        "-x",
        &x.to_string(),
        "-y",
        &y.to_string(),
    ])
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
    Ok(serde_json::from_slice(&out.stdout).context("bad niri json")?)
}
