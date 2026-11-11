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
        "clear-history" => {
            app.history.lock().unwrap().clear();
            app.status.send_modify(|st| st.last_heard_ms = None);
            app.log("info", "History cleared from the UI");
        }
        "reset" => {
            // Back to a fresh install, keeping downloaded Quran recitations. The files go first,
            // then the process exits at once (no shutdown that could save settings again), and
            // systemd (or Docker's restart policy) starts it fresh.
            let dir = app.paths.settings.parent().map(std::path::Path::to_path_buf).ok_or_else(|| ApiError::bad("no data folder"))?;
            tracing::warn!("reset requested from the UI: removing everything in {} except quran/", dir.display());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await; // let the reply go out
                let removed = tokio::task::spawn_blocking(move || reset_data(&dir)).await.unwrap_or(0);
                tracing::warn!("reset: removed {removed} items; restarting");
                std::process::exit(0);
            });
        }
        "update" if !crate::platform::is_native() => {
            return Err(ApiError::bad(format!("This copy runs in Docker. Update it with: {DOCKER_UPDATE}")));
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
    if q.check && !crate::platform::is_native() {
        let tag = latest_release().await?;
        latest = json!({ "tag": tag, "available": true });
    } else if q.check {
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
        "platform": crate::platform::get().as_str(),
        "docker_update": DOCKER_UPDATE,
    })))
}

/// Remove everything in the data folder except the Quran recitations (`quran/`). Returns how
/// many entries were removed.
fn reset_data(dir: &std::path::Path) -> usize {
    let keep = |name: &std::ffi::OsStr| name == "quran";
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !keep(&e.file_name()))
        .filter(|e| {
            let p = e.path();
            // symlink_metadata: never follow a link out of the data folder.
            let is_dir = std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir());
            if is_dir { std::fs::remove_dir_all(&p).is_ok() } else { std::fs::remove_file(&p).is_ok() }
        })
        .count()
}

/// How a Docker install is updated (shown on the Updates page).
const DOCKER_UPDATE: &str = "docker compose pull && docker compose up -d";

/// The newest release tag on GitHub (containers have no `update.sh`). `LIVEMASJID_REPO` overrides the repository.
async fn latest_release() -> Result<String, ApiError> {
    let repo = std::env::var("LIVEMASJID_REPO").unwrap_or_else(|_| "MAYasin/Live-Masjid".into());
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let out = tokio::process::Command::new("curl")
        .args(["-sSL", "--fail", "--max-time", "20", "-H", "Accept: application/vnd.github+json"])
        .args(["-A", crate::integrations::USER_AGENT, &url])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|_| ApiError::bad("Couldn't reach GitHub to check for updates"))?;
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .and_then(|v| v.get("tag_name")?.as_str().map(str::to_string))
        .ok_or_else(|| ApiError::bad("Couldn't reach GitHub to check for updates"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reset_keeps_only_the_quran() {
        let dir = std::env::temp_dir().join(format!("lm-reset-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(dir.join("quran/abdulrahman-sudais-r1")).unwrap();
        std::fs::write(dir.join("quran/abdulrahman-sudais-r1/001.mp3"), b"x").unwrap();
        for f in ["settings.json", "history.jsonl", "quran-playlists.json", "site.json"] {
            std::fs::write(dir.join(f), b"{}").unwrap();
        }
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        assert_eq!(super::reset_data(&dir), 5);
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("quran")]);
        assert!(dir.join("quran/abdulrahman-sudais-r1/001.mp3").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
