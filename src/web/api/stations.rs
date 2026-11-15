//! Masjid directory and selection.

use crate::state::{App, PlayerCmd};
use crate::web::{ApiError, ApiResult, ok};
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

pub async fn list(State(app): State<App>) -> Json<Value> {
    let selected = app.settings().mount;
    let live = app.live.read().unwrap();
    let history = app.history.lock().unwrap();
    let last = history.all_last_heard();
    let catalog = app.catalog.read().unwrap();
    let entry = |mount: &str, name: &str, location: &str, genre: &str| {
        json!({
            "mount": mount, "name": name, "location": location, "genre": genre,
            "live": crate::monitor::is_live(&live, mount),
            "last_heard_ms": last.get(mount),
            "selected": selected.as_deref() == Some(mount),
        })
    };
    let mut stations: Vec<Value> = catalog.iter().map(|s| entry(&s.mount, &s.name, &s.location, &s.genre)).collect();
    // Live mounts that aren't in the directory (yet).
    for m in live.keys().filter(|m| !catalog.iter().any(|s| &s.mount == *m)) {
        stations.push(entry(m, m, "", "Other"));
    }
    Json(json!({
        "stations": stations,
        "fetched_ms": app.catalog_fetched_ms.load(Ordering::Relaxed),
    }))
}

pub async fn refresh(State(app): State<App>) -> ApiResult {
    let count = crate::catalog::refresh(&app).await?;
    Ok(Json(json!({ "count": count })))
}

#[derive(Deserialize)]
pub struct Select {
    mount: Option<String>,
}

fn valid_mount(m: &str) -> bool {
    m.len() <= 80 && m.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

pub async fn select(State(app): State<App>, Json(req): Json<Select>) -> ApiResult {
    let mount = req.mount.map(|m| m.trim().trim_start_matches('/').to_string()).filter(|m| !m.is_empty());
    if mount.as_deref().is_some_and(|m| !valid_mount(m)) {
        return Err(ApiError::bad("invalid mount"));
    }
    let name = mount.as_ref().map(|m| {
        let catalog = app.catalog.read().unwrap();
        catalog.iter().find(|s| &s.mount == m).map_or_else(|| m.clone(), |s| s.name.clone())
    });
    app.update_settings(|s| {
        s.mount = mount.clone();
        s.station_name = name.clone();
    })?;
    app.mount_gen.send_modify(|g| *g += 1);
    let _ = app.player_tx.send(PlayerCmd::Reconfigure);
    app.log("info", format!("Selected masjid: {}", name.as_deref().unwrap_or("none")));
    ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lm-stations-api-{}", crate::config::random_hex(6)));
        (crate::state::test_app(&dir), dir)
    }

    fn live_info() -> crate::state::LiveInfo {
        crate::state::LiveInfo { server_type: "audio/ogg".into(), stream_start: Some("now".into()), listeners: None, title: None }
    }

    #[tokio::test]
    async fn lists_the_directory_with_live_and_selected_flags() {
        let (app, dir) = app();
        *app.catalog.write().unwrap() = vec![
            crate::catalog::Station { mount: "a".into(), name: "Masjid A".into(), location: "Town".into(), genre: "Masjid".into() },
            crate::catalog::Station { mount: "b".into(), name: "Masjid B".into(), location: String::new(), genre: "Aalim".into() },
        ];
        {
            let mut live = app.live.write().unwrap();
            live.insert("a".into(), vec![("relay".into(), live_info())]);
            live.insert("newcomer".into(), vec![("relay".into(), live_info())]);
        }
        app.history.lock().unwrap().begin("b", "Masjid B", "opus", "poll");
        assert!(select(State(app.clone()), Json(Select { mount: Some("/b ".into()) })).await.is_ok());
        assert_eq!(app.settings().station_name.as_deref(), Some("Masjid B"), "named from the directory");

        let Json(v) = list(State(app.clone())).await;
        let stations = v["stations"].as_array().unwrap();
        assert_eq!(stations.len(), 3);
        assert_eq!(stations[0]["live"], true);
        assert_eq!(stations[0]["selected"], false);
        assert_eq!(stations[1]["selected"], true);
        assert!(stations[1]["last_heard_ms"].as_u64().is_some());
        assert_eq!(stations[2]["mount"], "newcomer", "live mounts missing from the directory are listed too");
        assert_eq!(stations[2]["genre"], "Other");

        assert!(select(State(app.clone()), Json(Select { mount: None })).await.is_ok());
        assert_eq!(app.settings().mount, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn refresh_respects_the_site_back_off() {
        let (app, dir) = app();
        app.site.refused(); // nothing may contact livemasjid.com now
        let Err(e) = refresh(State(app.clone())).await else { panic!("refreshed while refused") };
        assert!(e.1.contains("refusing"), "{}", e.1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mount_validation() {
        assert!(super::valid_mount("lakewood"));
        assert!(super::valid_mount("annoor-relay.mp3"));
        assert!(!super::valid_mount("../etc"));
        assert!(!super::valid_mount("a b"));
    }
}
