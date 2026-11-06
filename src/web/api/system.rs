//! Service-level actions: restart and update check.

use crate::state::App;
use crate::web::{ApiError, ApiResult, ok};
use axum::extract::{Path, State};
use std::time::Duration;

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
            let out = tokio::process::Command::new("sudo")
                .args(["-n", "/usr/bin/systemctl", "start", "--no-block", "livemasjid-update.service"])
                .output()
                .await
                .map_err(ApiError::bad)?;
            if !out.status.success() {
                return Err(ApiError::bad(String::from_utf8_lossy(&out.stderr).trim()));
            }
            app.log("info", "Update check started");
        }
        _ => return Err(ApiError::bad("unknown action")),
    }
    ok()
}
