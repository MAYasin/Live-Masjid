// "Now playing": selected masjid, playback controls, volume, health stats, recent broadcasts.

import { api } from "../api.js";
import { $, runAction, setStat } from "../dom.js";
import { clockTime, duration, lowerFirst, timeAgo } from "../format.js";
import { getStatus, subscribe } from "../store.js";
import { historyRow } from "./history.js";
import * as local from "../local.js";
import * as recordings from "./recordings.js";
import * as salah from "./salah.js";

const PHASES = {
  idle: "No masjid selected",
  waiting: "Waiting for broadcast",
  connecting: "Connecting",
  playing: "Live",
  reconnecting: "Reconnecting",
  stopped: "Stopped",
  recording: "Playing a recording",
};

let volumeTimer;
let salahMount = null;

const clock = (s) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;

export function init() {
  $("#play").addEventListener("click", (e) => runAction(e.currentTarget, () => api("player/play", {}), "Starting"));
  $("#stop").addEventListener("click", (e) =>
    runAction(e.currentTarget, () => api("player/stop", {}), "Stopped until the next broadcast"));
  $("#autoplay").addEventListener("change", (e) =>
    runAction(null, () => api("settings", { autoplay: e.target.checked }), e.target.checked ? "Auto-play on" : "Auto-play off"));
  $("#volume").addEventListener("input", () => {
    paintVolume();
    clearTimeout(volumeTimer);
    volumeTimer = setTimeout(() => runAction(null, () => api("settings", { volume: Number($("#volume").value) })), 120);
  });
  $("#rec-pause").addEventListener("click", (e) => {
    const paused = getStatus()?.recording?.paused;
    runAction(e.currentTarget, () => api("recordings/pause", { pause: !paused }));
  });
  $("#rec-stop").addEventListener("click", (e) => runAction(e.currentTarget, () => api("recordings/stop", {}), "Back to live radio"));
  $("#recordings-all").addEventListener("click", () => { location.hash = `recordings/${getStatus()?.mount ?? ""}`; });
  $("#recordings-latest").addEventListener("click", recordings.onPlayClick);
  $("#listen-here").addEventListener("click", (e) => {
    const s = getStatus();
    runAction(e.currentTarget, () => local.playLive(s?.mount, s?.station_name));
  });
  local.onChange(() => { const s = getStatus(); if (s) render(s); });
  subscribe(render);
  // Countdown to the next salah, and a periodic refresh (the Pi serves it from cache).
  setInterval(salah.tick, 30 * 1000);
  setInterval(() => salah.load(salahMount), 30 * 60 * 1000);
}

export function show() {
  loadRecent();
  loadRecordings();
  salah.load(getStatus()?.mount);
}

async function loadRecordings() {
  const mount = getStatus()?.mount;
  const box = $("#recordings-latest");
  if (!mount) { box.innerHTML = '<div class="empty small">Choose a masjid to see its recordings.</div>'; return; }
  try {
    const list = await api(`recordings?mount=${encodeURIComponent(mount)}`);
    box.innerHTML = list.status === "ok" ? recordings.rows(list, 4) : recordings.fallback(list);
  } catch (err) {
    box.innerHTML = recordings.fallback({ message: err.message });
  }
}

function paintVolume() {
  const v = $("#volume").value;
  $("#volume").style.setProperty("--fill", `${v}%`);
  $("#volume-value").textContent = v;
}

function detailLine(s) {
  const out = s.output;
  switch (s.phase) {
    case "playing":
      return `Playing for ${duration(Date.now() - s.phase_since_ms)} · ${String(s.codec ?? "").toUpperCase()} · ${out.open ? out.label : "no output"}`;
    case "reconnecting":
      return s.buffer_ms > 1000 ? "Connection dropped; still playing from the buffer" : "Connection dropped; reconnecting";
    case "stopped":
      return "Resumes automatically at the next broadcast";
    case "connecting":
      return "Opening stream…";
    default:
      return `Last heard ${lowerFirst(timeAgo(s.last_heard_ms))}`;
  }
}

function render(s) {
  const active = ["playing", "connecting", "reconnecting"].includes(s.phase);
  const rec = s.recording;
  if (s.mount !== salahMount) {
    salahMount = s.mount;
    salah.load(s.mount);
    loadRecordings();
  }
  $("#no-selection").hidden = Boolean(s.mount);
  $("#now-panel").hidden = !s.mount;
  $("#now-panel").className = `panel phase-${s.phase}`;
  $("#phase").textContent = PHASES[s.phase] ?? s.phase;
  $("#station-name").textContent = rec ? rec.title : (s.station_name || s.mount || "—");
  $("#station-location").textContent = s.station_location ?? "";
  $("#station-location").hidden = !s.station_location || Boolean(s.recording);
  $("#station-detail").textContent = rec ? `Recording · ${rec.paused ? "paused" : "playing on the speaker"}` : detailLine(s);
  $("#play").hidden = active || Boolean(rec);
  $("#stop").hidden = !active;
  $("#rec-pause").hidden = !rec;
  $("#rec-stop").hidden = !rec;
  $("#rec-progress").hidden = !rec;
  const onAir = s.signals.poll_live || ["playing", "reconnecting"].includes(s.phase);
  $("#listen-here").hidden = !s.browser_listening || !onAir || local.isPlaying();
  if (rec) {
    $("#rec-pause").textContent = rec.paused ? "Resume" : "Pause";
    const total = rec.duration_s;
    $("#rec-time").textContent = total ? `${clock(rec.position_s)} / ${clock(total)}` : clock(rec.position_s);
    $("#rec-bar").style.width = total ? `${Math.min(100, (100 * rec.position_s) / total)}%` : "0%";
  }
  $("#autoplay").checked = s.autoplay;
  if (document.activeElement !== $("#volume")) $("#volume").value = s.volume;
  paintVolume();

  const out = s.output;
  const sig = s.signals;
  const battery = out.kind === "bluetooth" && s.bt.battery != null ? ` · ${s.bt.battery}%` : "";
  setStat("stat-output", out.open ? `${out.label}${out.kind === "bluetooth" ? " (Bluetooth)" : ""}${battery}` : out.error ? "Unavailable" : "Idle",
    out.open ? "ok" : out.error ? "err" : "");
  if (s.site_blocked_until_ms) setStat("stat-signal", "Paused", "warn");
  else if (s.phase !== "waiting") setStat("stat-signal", s.mount ? "Standby" : "—");
  else setStat("stat-signal", sig.sse_connected ? "Listening" : "Reconnecting", sig.sse_connected ? "ok" : "warn");
  setStat("stat-poll", sig.poll_ok ? `OK, ${lowerFirst(timeAgo(sig.poll_last_ms))}` : "Failing", sig.poll_ok ? "ok" : "err");
  if (active) {
    const secs = Math.round(s.buffer_ms / 1000);
    $("#stat-extra-key").textContent = "Buffer";
    setStat("stat-extra", `${secs} s of audio`, secs >= 5 ? "ok" : "warn");
  } else {
    $("#stat-extra-key").textContent = "Streams live";
    setStat("stat-extra", `${s.live_count}${s.mount && sig.poll_live ? " · yours is live" : ""}`, sig.poll_live ? "ok" : "");
  }

  const blocked = $("#site-blocked");
  blocked.hidden = !s.site_blocked_until_ms;
  if (s.site_blocked_until_ms) {
    blocked.innerHTML = `<b>livemasjid.com is refusing requests from this network.</b> Playback is unaffected: it uses the
      public stream servers. The instant live signal and directory updates are paused until about
      ${clockTime(s.site_blocked_until_ms)}. If this persists, ask the livemasjid team to lift the block.`;
  }
  document.title = s.phase === "playing" ? `● ${s.station_name ?? "Live Masjid"}` : "Live Masjid";
}

async function loadRecent() {
  try {
    const { sessions } = await api("history?limit=5");
    $("#recent").innerHTML = sessions.length
      ? sessions.map(historyRow).join("")
      : '<div class="empty">No broadcasts recorded yet. They appear here automatically.</div>';
  } catch { /* shown via connection indicator */ }
}
