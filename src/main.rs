//! Live Masjid Pi — always-on auto-player for livemasjid.com streams.

mod bluetooth;
mod catalog;
mod config;
mod history;
mod integrations;
mod monitor;
mod net;
mod player;
mod sink;
mod site;
mod sysinfo;
mod state;
mod stream;
mod web;

use state::{AppState, Phase, Status, now_ms};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub const VERSION: &str = match option_env!("LIVEMASJID_BUILD") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("livemasjid {VERSION}");
        return Ok(());
    }
    // Dev helper: `livemasjid --parse-catalog index.html` prints the parsed directory as JSON.
    if let Some(i) = std::env::args().position(|a| a == "--parse-catalog") {
        let path = std::env::args().nth(i + 1).ok_or_else(|| anyhow::anyhow!("missing file"))?;
        let html = std::fs::read_to_string(path)?;
        println!("{}", serde_json::to_string(&catalog::parse(&html))?);
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LIVEMASJID_LOG").unwrap_or_else(|_| "livemasjid=info".into()),
        )
        // journald adds its own timestamps
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .init();

    #[cfg(feature = "tls")]
    let _ = rustls::crypto::ring::default_provider().install_default();

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(512 * 1024)
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let paths = config::Paths::from_env();
    let paths_site = paths.site.clone();
    let data_dir = paths.site.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    let settings = config::Settings::load(&paths.settings);
    stream::LIVE_DELAY_MS.store(u64::from(settings.live_delay_s) * 1000, std::sync::atomic::Ordering::Relaxed);
    settings.save(&paths.settings)?; // persist generated secret / migrated defaults
    tracing::info!("livemasjid {VERSION} starting; data in {:?}", paths.settings.parent());

    let volume = Arc::new(AtomicU8::new(settings.volume));
    let sink = sink::SinkHandle::spawn(volume.clone(), settings.keep_awake);
    let (player_tx, player_rx) = tokio::sync::mpsc::unbounded_channel();
    let (signal_tx, signal_rx) = tokio::sync::mpsc::unbounded_channel();
    let history = history::History::load(paths.history.clone());

    let status = Status {
        version: VERSION,
        phase: if settings.mount.is_some() { Phase::Waiting } else { Phase::Idle },
        mount: settings.mount.clone(),
        station_name: settings.station_name.clone(),
        station_location: None,
        phase_since_ms: now_ms(),
        codec: None,
        stream_url: None,
        trigger: None,
        last_heard_ms: settings.mount.as_deref().and_then(|m| history.last_heard(m)),
        autoplay: settings.autoplay,
        volume: settings.volume,
        keep_awake: settings.keep_awake,
        signals: Default::default(),
        output: Default::default(),
        bt: state::BtStatus { device: settings.bt_device.clone(), name: settings.bt_name.clone(), ..Default::default() },
        password_set: settings.password_hash.is_some(),
        started_ms: now_ms(),
        live_count: 0,
        site_blocked_until_ms: None,
        recording: None,
        browser_listening: settings.browser_listening,
        buffer_ms: 0,
    };

    let app = Arc::new(AppState {
        paths,
        settings: RwLock::new(settings),
        status: tokio::sync::watch::Sender::new(status),
        catalog: RwLock::new(Vec::new()),
        catalog_fetched_ms: AtomicU64::new(0),
        live: RwLock::new(Default::default()),
        history: Mutex::new(history),
        logs: Mutex::new(Default::default()),
        player_tx,
        signal_tx,
        mount_gen: tokio::sync::watch::Sender::new(0),
        volume,
        http: net::build_client(),
        sink,
        hb_player: AtomicU64::new(now_ms()),
        site: site::SiteGuard::load(paths_site),
        integrations: integrations::Integrations::new(&data_dir),
    });

    {
        let weak = Arc::downgrade(&app);
        // Log output *changes* only (opened on X / lost X), not every retry or raw ALSA text.
        let last_logged: Mutex<Option<String>> = Mutex::new(None);
        app.sink.on_status(move |out| {
            if let Some(app) = weak.upgrade() {
                let key = out.open.then(|| out.device.clone());
                let mut last = last_logged.lock().unwrap();
                if *last != key {
                    match (&key, last.as_ref()) {
                        (Some(_), _) => app.log("info", format!("Audio playing on {} ({})", out.label, out.kind)),
                        (None, Some(_)) => app.log("warn", "Audio output lost; retrying"),
                        (None, None) => {}
                    }
                    *last = key;
                }
                app.status.send_modify(|s| s.output = out);
            }
        });
    }

    catalog::load_cache(&app);
    let _ = app.update_settings(|_| {}); // fill derived status (location, last heard)
    app.sink.retarget(bluetooth::compute_targets(&app));
    app.log("info", format!("Live Masjid {VERSION} started"));

    tokio::spawn(player::run(app.clone(), player_rx, signal_rx));
    tokio::spawn(monitor::sse::run(app.clone()));
    tokio::spawn(monitor::poll::run(app.clone()));
    tokio::spawn(catalog::run(app.clone()));
    tokio::spawn(bluetooth::run(app.clone()));
    tokio::spawn(watchdog(app.clone()));

    let port: u16 = std::env::var("LIVEMASJID_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(80);
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("cannot bind port {port} ({e}); using 8080");
            tokio::net::TcpListener::bind(("0.0.0.0", 8080)).await?
        }
    };
    tracing::info!("web UI on http://{}", listener.local_addr()?);
    sd_notify("READY=1");

    let server = axum::serve(listener, web::router(app.clone()));
    tokio::select! {
        r = server => r?,
        _ = shutdown_signal() => tracing::info!("shutting down"),
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("sigterm");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// systemd watchdog: only pet it while the player loop and audio thread are alive.
async fn watchdog(app: state::App) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let now = now_ms();
        let p = now.saturating_sub(app.hb_player.load(Ordering::Relaxed));
        let s = now.saturating_sub(app.sink.heartbeat.load(Ordering::Relaxed));
        let blocked = app.site.blocked_until();
        app.status.send_if_modified(|st| std::mem::replace(&mut st.site_blocked_until_ms, blocked) != blocked);
        if p < 30_000 && s < 30_000 {
            sd_notify("WATCHDOG=1");
        } else {
            tracing::error!("watchdog: player {p} ms / sink {s} ms since heartbeat");
        }
    }
}

#[cfg(target_os = "linux")]
fn sd_notify(msg: &str) {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let path = path.to_string_lossy().into_owned();
    let addr = if let Some(abs) = path.strip_prefix('@') {
        SocketAddr::from_abstract_name(abs.as_bytes())
    } else {
        SocketAddr::from_pathname(&path)
    };
    if let (Ok(sock), Ok(addr)) = (UnixDatagram::unbound(), addr) {
        let _ = sock.send_to_addr(msg.as_bytes(), &addr);
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_notify(_msg: &str) {}
