//! Service-level actions (restart, update now) and the update status for the System page.
//!
//! Updates are installed by `livemasjid-update.service` (root, via an exact sudo rule). "Update now"
//! leaves `<data>/update-request` so `update.sh` treats that run as `--force` (even over a test
//! build); `update.sh` reports progress in `<data>/update-status.json`. Automatic (timer) updates
//! follow the `auto_update` setting, which `update.sh` reads from `settings.json`.

use crate::state::App;
use crate::sync::LockExt;
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
    let mut cmd = tokio::process::Command::new("sudo");
    cmd.args(["-n", "/usr/bin/systemctl", "start", "--no-block", "livemasjid-update.service"]);
    run_checked(cmd).await
}

/// Run `cmd`; when it fails, its stderr is the error.
async fn run_checked(mut cmd: tokio::process::Command) -> Result<(), ApiError> {
    let out = cmd.output().await.map_err(ApiError::bad)?;
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
            app.history.locked().clear();
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
    let last: Value =
        std::fs::read(data_file(&app, "update-status.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let mut latest = Value::Null;
    if q.check && !crate::platform::is_native() {
        let tag = latest_release().await?;
        latest = json!({ "tag": tag, "available": true });
    } else if q.check {
        let mut cmd = tokio::process::Command::new("/opt/livemasjid/update.sh");
        cmd.arg("--check");
        latest = check_with(cmd).await?;
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

/// Run `update.sh --check` (given as `cmd`) and read its answer.
async fn check_with(mut cmd: tokio::process::Command) -> Result<Value, ApiError> {
    let out = tokio::time::timeout(Duration::from_secs(45), cmd.kill_on_drop(true).output()).await;
    match out {
        Ok(Ok(o)) => parse_check(&String::from_utf8_lossy(&o.stdout)),
        _ => Err(ApiError::bad("Couldn't reach GitHub to check for updates")),
    }
}

/// `update.sh --check` prints `latest=<tag>` and `asset_available=yes|no`, or an explanation.
fn parse_check(text: &str) -> Result<Value, ApiError> {
    let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::to_string);
    match field("latest") {
        Some(tag) => Ok(json!({ "tag": tag, "available": field("asset_available").as_deref() == Some("yes") })),
        None => Err(ApiError::bad(text.lines().last().unwrap_or("Couldn't check for updates").to_string())),
    }
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
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args(["-sSL", "--fail", "--max-time", "20", "-H", "Accept: application/vnd.github+json"]).args([
        "-A",
        crate::integrations::USER_AGENT,
        &url,
    ]);
    release_tag(cmd).await
}

/// Run the request `cmd` and read `tag_name` from the GitHub release JSON it prints.
async fn release_tag(mut cmd: tokio::process::Command) -> Result<String, ApiError> {
    let unreachable = || ApiError::bad("Couldn't reach GitHub to check for updates");
    let out = cmd.kill_on_drop(true).output().await.map_err(|_| unreachable())?;
    serde_json::from_slice::<Value>(&out.stdout).ok().and_then(|v| v.get("tag_name")?.as_str().map(str::to_string)).ok_or_else(unreachable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    fn app() -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lm-system-api-{}", crate::config::random_hex(6)));
        (crate::state::test_app(&dir), dir)
    }

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[tokio::test]
    async fn clear_history_and_unknown_actions() {
        let (app, dir) = app();
        app.history.locked().begin("m", "M", "opus", "manual");
        app.status.send_modify(|s| s.last_heard_ms = Some(5));
        assert!(action(State(app.clone()), Path("clear-history".into())).await.is_ok());
        assert!(app.history.locked().recent(10, None).is_empty());
        assert_eq!(app.status.borrow().last_heard_ms, None);
        let Err(e) = action(State(app.clone()), Path("format-disk".into())).await else { panic!("unknown action accepted") };
        assert_eq!(e.0, axum::http::StatusCode::BAD_REQUEST);
        let _ = std::fs::remove_dir_all(dir);
    }

    // Restart and reset exit the process 300 ms later from a spawned task. On this
    // current-thread test runtime that task never runs: the handler doesn't yield, so the test
    // ends (and the runtime drops the task) before it is polled.
    #[tokio::test(flavor = "current_thread")]
    async fn restart_is_acknowledged() {
        let (app, dir) = app();
        assert!(action(State(app.clone()), Path("restart".into())).await.is_ok());
        assert!(app.logs.locked().iter().any(|l| l.msg.contains("Restart")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reset_is_acknowledged() {
        let (app, dir) = app();
        assert!(action(State(app.clone()), Path("reset".into())).await.is_ok());
        assert!(dir.join("settings.json").exists(), "nothing is removed before the reply");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn update_in_a_container_explains_docker() {
        if crate::platform::is_native() {
            return; // would start the real update service
        }
        let (app, dir) = app();
        let Err(e) = action(State(app.clone()), Path("update".into())).await else { panic!("update in a container") };
        assert!(e.1.contains(DOCKER_UPDATE));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn updates_reports_the_last_run() {
        let (app, dir) = app();
        let Json(v) = updates(State(app.clone()), Query(UpdatesQuery { check: false })).await.ok().expect("updates");
        assert_eq!(v["last"], Value::Null);
        assert_eq!(v["latest"], Value::Null);
        assert_eq!(v["installed"], crate::VERSION);
        assert_eq!(v["docker_update"], DOCKER_UPDATE);
        std::fs::write(data_file(&app, "update-status.json"), br#"{"state":"up_to_date"}"#).unwrap();
        let Json(v) = updates(State(app.clone()), Query(UpdatesQuery { check: false })).await.ok().expect("updates");
        assert_eq!(v["last"]["state"], "up_to_date");
        std::fs::write(data_file(&app, "update-status.json"), b"not json").unwrap();
        let Json(v) = updates(State(app.clone()), Query(UpdatesQuery { check: false })).await.ok().expect("updates");
        assert_eq!(v["last"], Value::Null, "a broken status file is ignored");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn check_output_is_parsed() {
        let v = parse_check("checking\nlatest=v1.2\nasset_available=yes\n").ok().expect("parsed");
        assert_eq!(v, json!({ "tag": "v1.2", "available": true }));
        let v = parse_check("latest=v1.2\nasset_available=no").ok().expect("parsed");
        assert_eq!(v["available"], false);
        let Err(e) = parse_check("GitHub said no\nrate limited") else { panic!("no tag") };
        assert_eq!(e.1, "rate limited");
        let Err(e) = parse_check("") else { panic!("no tag") };
        assert_eq!(e.1, "Couldn't check for updates");
    }

    #[tokio::test]
    async fn check_runs_the_script() {
        let v = check_with(sh("echo latest=v2; echo asset_available=yes")).await.ok().expect("checked");
        assert_eq!(v["tag"], "v2");
        let Err(e) = check_with(sh("echo offline")).await else { panic!("no tag") };
        assert_eq!(e.1, "offline");
        let Err(e) = check_with(Command::new("/nonexistent/update.sh")).await else { panic!("missing script") };
        assert!(e.1.contains("Couldn't reach GitHub"));
    }

    #[tokio::test]
    async fn commands_report_their_stderr() {
        assert!(run_checked(sh("exit 0")).await.is_ok());
        let Err(e) = run_checked(sh("echo ' not allowed ' >&2; exit 1")).await else { panic!("failed command") };
        assert_eq!(e.1, "not allowed");
        assert!(run_checked(Command::new("/nonexistent/sudo")).await.is_err());
    }

    #[tokio::test]
    async fn release_tag_reads_the_github_json() {
        assert_eq!(release_tag(sh(r#"echo '{"tag_name":"v3.0"}'"#)).await.ok().as_deref(), Some("v3.0"));
        assert!(release_tag(sh(r#"echo '{"message":"Not Found"}'"#)).await.is_err());
        assert!(release_tag(sh("echo garbage")).await.is_err());
        assert!(release_tag(Command::new("/nonexistent/curl")).await.is_err());
    }

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
