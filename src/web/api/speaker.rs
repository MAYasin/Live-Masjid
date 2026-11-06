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
    tokio::spawn(bluetooth::scan(app.clone(), 20));
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

pub async fn release(State(app): State<App>, Json(r): Json<Mac>) -> ApiResult {
    bluetooth::release(&app, &r.mac, false).await?;
    ok()
}

pub async fn forget(State(app): State<App>, Json(r): Json<Mac>) -> ApiResult {
    bluetooth::release(&app, &r.mac, true).await?;
    ok()
}

pub async fn outputs() -> ApiResult {
    let cards: Vec<_> = bluetooth::wired_cards()
        .into_iter()
        .map(|(id, label)| json!({ "device": format!("default:CARD={id}"), "label": label }))
        .collect();
    Ok(Json(json!({ "cards": cards })))
}
