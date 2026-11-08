//! Service-level actions (restart, update now) and the update status for the System page.
//!
//! Updates are installed by `livemasjid-update.service` (root, via an exact sudo rule). "Update now"
//! leaves `<data>/update-request` so `update.sh` treats that run as `--force` (even over a test
//! build); `update.sh` reports progress in `<data>/update-status.json`. Automatic (timer) updates
//! follow the `auto_update` setting, which `update.sh` reads from `settings.json`.

use crate::state::App;
use crate::web::{ApiError, ApiResult, ok};
use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

fn data_file(app: &App, name: &str) -> std::path::PathBuf {
    app.paths.settings.with_file_name(name)
}

async fn start_update_service() -> Result<(), ApiError> {
    let out = tokio::process::Command::new("sudo")
        .args(["-n", "/usr/bin/systemctl", "start", "--no-block", "livemasjid-update.service"])
        .output()
        .await
        .map_err(ApiError::bad)?;
    if !out.status.success() {
        return Err(ApiError::bad(String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

pub async fn action(State(app): State<App>, Path(action): Path<String>) -> ApiResult {
    match action.as_str() {
        "restart" => {
            app.log("info", "Restart requested from the UI");
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                std::process::exit(0); // systemd restarts us
            });
        }
        "update" => {
            // Marks this run as requested by a person: install even over a test build.
            std::fs::write(data_file(&app, "update-request"), b"1").map_err(ApiError::bad)?;
            start_update_service().await?;
            app.log("info", "Update requested from the UI");
        }
        _ => return Err(ApiError::bad("unknown action")),
    }
    ok()
}

#[derive(Deserialize)]
pub struct UpdatesQuery {
    /// Also ask GitHub for the latest release (only when the user presses "Check").
    #[serde(default)]
    check: bool,
}

/// Installed version, automatic-update setting, the last update run, and (on request) the latest release.
pub async fn updates(State(app): State<App>, Query(q): Query<UpdatesQuery>) -> ApiResult<Json<Value>> {
    let last: Value = std::fs::read(data_file(&app, "update-status.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let mut latest = Value::Null;
    if q.check {
        let out = tokio::time::timeout(
            Duration::from_secs(45),
            tokio::process::Command::new("/opt/livemasjid/update.sh").arg("--check").kill_on_drop(true).output(),
        )
        .await;
        let text = match out {
            Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return Err(ApiError::bad("Couldn't reach GitHub to check for updates")),
        };
        let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::to_string);
        match field("latest") {
            Some(tag) => latest = json!({ "tag": tag, "available": field("asset_available").as_deref() == Some("yes") }),
            None => return Err(ApiError::bad(text.lines().last().unwrap_or("Couldn't check for updates").to_string())),
        }
    }
    Ok(Json(json!({
        "installed": crate::VERSION,
        "auto_update": app.settings().auto_update,
        "last": last,
        "latest": latest,
    })))
}
