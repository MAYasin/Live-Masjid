// "Now playing": selected masjid, playback controls, volume, health stats, recent broadcasts.

import { api } from "../api.js";
import { $, runAction, setStat } from "../dom.js";
import { clockTime, duration, lowerFirst, timeAgo } from "../format.js";
import { subscribe } from "../store.js";
import { historyRow } from "./history.js";

const PHASES = {
  idle: "No masjid selected",
  waiting: "Waiting for broadcast",
  connecting: "Connecting",
  playing: "Live",
  reconnecting: "Reconnecting",
  stopped: "Stopped",
};

let volumeTimer;

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
  subscribe(render);
}

export function show() {
  loadRecent();
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
  $("#no-selection").hidden = Boolean(s.mount);
  $("#now-panel").hidden = !s.mount;
  $("#now-panel").className = `panel phase-${s.phase}`;
  $("#phase").textContent = PHASES[s.phase] ?? s.phase;
  $("#station-name").textContent = s.station_name || s.mount || "—";
  $("#station-location").textContent = s.station_location ?? "";
  $("#station-location").hidden = !s.station_location;
  $("#station-detail").textContent = detailLine(s);
  $("#play").hidden = active;
  $("#stop").hidden = !active;
  $("#autoplay").checked = s.autoplay;
  if (document.activeElement !== $("#volume")) $("#volume").value = s.volume;
  paintVolume();

  const out = s.output;
  const sig = s.signals;
  setStat("stat-output", out.open ? `${out.label}${out.kind === "bluetooth" ? " (Bluetooth)" : ""}` : out.error ? "Unavailable" : "Idle",
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
