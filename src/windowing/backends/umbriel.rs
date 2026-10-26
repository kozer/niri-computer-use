//! Window backend for the umbriel wlroots compositor (`umbriel` CLI).
//!
//! `umbriel windows --json` reports global logical geometry. Window ids are
//! 128-bit hex strings, so `window_id` is their first 16 hex digits and the
//! full id is recovered by prefix when acting on a window.

use crate::command_runner;
use crate::terminal::enrich_terminal_windows;
use crate::windowing::registry::BackendProbe;
use crate::windowing::types::{WindowBounds, WindowInfo};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::process::Command as StdCommand;

pub const UMBRIEL_BACKEND: &str = "umbriel";

#[derive(Debug, Deserialize)]
struct UmbrielWindow {
    id: String,
    #[serde(default)]
    app_id: String,
    #[serde(default)]
    title: String,
    pid: Option<i64>,
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default)]
    w: i32,
    #[serde(default)]
    h: i32,
    #[serde(default)]
    output: String,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    focused: bool,
}

#[derive(Debug, Deserialize)]
struct UmbrielOutput {
    name: String,
    #[serde(default = "one")]
    scale: f64,
    position: Position,
    logical_size: Size,
    #[serde(default)]
    active_workspace: String,
}

#[derive(Debug, Deserialize)]
struct Position {
    x: i32,
    y: i32,
}

#[derive(Debug, Deserialize)]
struct Size {
    width: i32,
    height: i32,
}

fn one() -> f64 {
    1.0
}

/// First 16 hex digits of an umbriel id, as the numeric window id.
fn short_id(id: &str) -> Option<u64> {
    u64::from_str_radix(id.get(..16)?, 16).ok()
}

fn umbriel_json(args: &[&str]) -> Result<String> {
    let mut command = StdCommand::new("umbriel");
    command.args(args);
    let output = command_runner::output_blocking(&mut command, "run umbriel")?;
    if !output.status.success() {
        bail!(
            "umbriel {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn read_windows() -> Result<Vec<UmbrielWindow>> {
    serde_json::from_str(&umbriel_json(&["windows", "--json"])?)
        .context("failed to parse the umbriel window list")
}

fn read_outputs() -> Result<Vec<UmbrielOutput>> {
    serde_json::from_str(&umbriel_json(&["outputs", "--json"])?)
        .context("failed to parse the umbriel output list")
}

pub fn probe() -> BackendProbe {
    match read_windows() {
        Ok(windows) => BackendProbe {
            id: UMBRIEL_BACKEND,
            ok: true,
            can_list_windows: true,
            can_focus_apps: true,
            can_focus_windows: true,
            detail: format!("umbriel windows returned {} window(s)", windows.len()),
        },
        Err(error) => BackendProbe {
            id: UMBRIEL_BACKEND,
            ok: false,
            can_list_windows: false,
            can_focus_apps: false,
            can_focus_windows: false,
            detail: format!("{error:#}"),
        },
    }
}

pub async fn list_windows() -> Result<Vec<WindowInfo>> {
    tokio::task::spawn_blocking(list_windows_blocking)
        .await
        .context("umbriel window listing task panicked")?
}

fn list_windows_blocking() -> Result<Vec<WindowInfo>> {
    let windows = read_windows()?;
    // Pixel bounds equal logical geometry only at scale 1 on every output.
    let unit_scale = read_outputs().is_ok_and(|outputs| outputs.iter().all(|o| o.scale == 1.0));
    let mut infos = windows
        .into_iter()
        .filter_map(|window| {
            let window_id = short_id(&window.id)?;
            let bounds = (unit_scale && window.w > 0 && window.h > 0).then(|| WindowBounds {
                x: Some(window.x),
                y: Some(window.y),
                width: window.w as u32,
                height: window.h as u32,
            });
            let app_id = Some(window.app_id).filter(|value| !value.is_empty());
            Some(WindowInfo {
                window_id,
                title: Some(window.title),
                wm_class: app_id.clone(),
                app_id,
                pid: window.pid.and_then(|pid| u32::try_from(pid).ok()),
                bounds,
                workspace: window
                    .workspace
                    .rsplit(':')
                    .next()
                    .and_then(|index| index.parse().ok()),
                focused: window.focused,
                hidden: false,
                client_type: None,
                backend: UMBRIEL_BACKEND.to_string(),
                terminal: None,
            })
        })
        .collect::<Vec<_>>();
    infos.sort_by_key(|window| window.window_id);
    enrich_terminal_windows(&mut infos);
    Ok(infos)
}

/// Focus an exact umbriel window. umbriel switches to the window's workspace.
pub async fn activate_window(window_id: u64) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let full = read_windows()?
            .into_iter()
            .find(|window| short_id(&window.id) == Some(window_id))
            .map(|window| window.id)
            .ok_or_else(|| anyhow!("umbriel lists no window {window_id:016x}"))?;
        umbriel_json(&["msg", &format!("window-focus:{full}")]).map(|_| ())
    })
    .await
    .context("umbriel focus task panicked")?
}

/// Whether the window's rectangle lies on an output whose active workspace
/// holds it. Scrolled-out columns and windows on other workspaces are not.
pub async fn on_screen(window_id: u64) -> Result<bool> {
    tokio::task::spawn_blocking(move || {
        let Some(window) = read_windows()?
            .into_iter()
            .find(|window| short_id(&window.id) == Some(window_id))
        else {
            return Ok(false);
        };
        Ok(read_outputs()?.iter().any(|output| {
            output.name == window.output
                && output.active_workspace == window.workspace
                && window.x >= output.position.x
                && window.y >= output.position.y
                && window.x + window.w <= output.position.x + output.logical_size.width
                && window.y + window.h <= output.position.y + output.logical_size.height
        }))
    })
    .await
    .context("umbriel output lookup task panicked")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_is_the_first_sixteen_hex_digits() {
        assert_eq!(
            short_id("b57e6b30dcd2e5bf12681fa3eeb89394"),
            Some(0xb57e6b30dcd2e5bf)
        );
        assert_eq!(short_id("zz"), None);
    }

    #[test]
    fn reads_windows_from_a_live_capture() {
        let json = r#"[{"app_id":"kitty","title":"t","pid":7,"x":8,"y":56,"w":2544,"h":1376,
            "output":"HDMI-A-1","workspace":"HDMI-A-1:1","focused":true,"visible":true,
            "id":"b57e6b30dcd2e5bf12681fa3eeb89394","extra":1}]"#;
        let windows: Vec<UmbrielWindow> = serde_json::from_str(json).unwrap();
        assert_eq!(windows[0].w, 2544);
        assert!(windows[0].focused);
    }
}
