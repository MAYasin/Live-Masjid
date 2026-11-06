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
    #[test]
    fn mount_validation() {
        assert!(super::valid_mount("lakewood"));
        assert!(super::valid_mount("annoor-relay.mp3"));
        assert!(!super::valid_mount("../etc"));
        assert!(!super::valid_mount("a b"));
    }
}
