//! Bluetooth speakers and audio outputs.

use crate::bluetooth;
use crate::state::App;
use crate::web::{ApiResult, ok};
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::json;

pub async fn devices(State(app): State<App>) -> ApiResult {
    Ok(Json(json!({ "devices": bluetooth::list(&app).await? })))
}

pub async fn scan(State(app): State<App>) -> ApiResult {
    tokio::spawn(bluetooth::scan(app, 20));
    ok()
}

#[derive(Deserialize)]
pub struct Mac {
    mac: String,
}

pub async fn use_device(State(app): State<App>, Json(r): Json<Mac>) -> ApiResult {
    let message = bluetooth::use_device(&app, &r.mac).await?;
    Ok(Json(json!({ "ok": true, "message": message })))
}

pub async fn repair(State(app): State<App>, Json(r): Json<Mac>) -> ApiResult {
    let message = bluetooth::repair(&app, &r.mac).await?;
    Ok(Json(json!({ "ok": true, "message": message })))
}

pub async fn release(State(app): State<App>, Json(r): Json<Mac>) -> ApiResult {
    bluetooth::release(&app, &r.mac, false).await?;
    ok()
}

pub async fn forget(State(app): State<App>, Json(r): Json<Mac>) -> ApiResult {
    bluetooth::release(&app, &r.mac, true).await?;
    ok()
}

pub async fn outputs() -> ApiResult {
    let cards: Vec<_> =
        bluetooth::wired_cards().into_iter().map(|(id, label)| json!({ "device": format!("default:CARD={id}"), "label": label })).collect();
    Ok(Json(json!({ "cards": cards })))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only the validation paths: a malformed address is refused before bluetoothctl runs.
    #[tokio::test]
    async fn malformed_addresses_are_refused() {
        let dir = std::env::temp_dir().join(format!("lm-speaker-api-{}", crate::config::random_hex(6)));
        let app = crate::state::test_app(&dir);
        let mac = || Json(Mac { mac: "not-a-mac; reboot".into() });
        assert!(use_device(State(app.clone()), mac()).await.is_err());
        assert!(repair(State(app.clone()), mac()).await.is_err());
        assert!(release(State(app.clone()), mac()).await.is_err());
        assert!(forget(State(app.clone()), mac()).await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn outputs_list_wired_cards() {
        let Json(v) = outputs().await.ok().expect("outputs");
        for card in v["cards"].as_array().expect("cards") {
            assert!(card["device"].as_str().is_some_and(|d| d.starts_with("default:CARD=")));
        }
    }
}
