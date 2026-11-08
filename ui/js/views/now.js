// "Now playing": selected masjid, playback controls, volume, health stats, recent broadcasts.

import { api } from "../api.js";
import { $, runAction } from "../dom.js";
import { clockTime, duration, lowerFirst, timeAgo } from "../format.js";
import { getStatus, subscribe } from "../store.js";
import * as local from "../local.js";
import * as miniplayer from "../miniplayer.js";
import * as recordings from "./recordings.js";
import * as salah from "./salah.js";

const PHASES = {
  idle: "No masjid selected",
  waiting: "Waiting for broadcast",
  connecting: "Connecting",
  playing: "Live",
  reconnecting: "Reconnecting",
  stopped: "Stopped",
  recording: "Waiting for broadcast",
};

let volumeTimer;
let salahMount = null;

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
  miniplayer.mount($("#now-player"));
  $("#recordings-all").addEventListener("click", () => { location.hash = `recordings/${getStatus()?.mount ?? ""}`; });
  $("#recordings-latest").addEventListener("click", recordings.onPlayClick);
  $("#listen-here").addEventListener("click", (e) => {
    const s = getStatus();
    runAction(e.currentTarget, () => local.playLive(s?.mount, s?.recording?.title ?? s?.station_name));
  });
  local.onChange(() => { const s = getStatus(); if (s) render(s); });
  subscribe(render);
  salah.init();
  // Countdown to the next salah, and a periodic refresh (the Pi serves it from cache).
  setInterval(salah.tick, 30 * 1000);
  setInterval(() => salah.load(salahMount), 30 * 60 * 1000);
}

export function show() {
  loadRecordings();
  salah.load(getStatus()?.mount);
}

async function loadRecordings() {
  const mount = getStatus()?.mount;
  const box = $("#recordings-latest");
  if (!mount) { box.innerHTML = '<div class="empty small">Choose a masjid to see its recordings.</div>'; return; }
  try {
    const list = await api(`recordings?mount=${encodeURIComponent(mount)}`);
    box.innerHTML = list.status === "ok" ? recordings.rows(list, 3) : recordings.fallback(list);
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
  // The masjid card is always about the broadcast; the Quran or a recording has its own card.
  $("#station-name").textContent = s.station_name || s.mount || "—";
  $("#station-location").textContent = s.station_location ?? "";
  $("#station-location").hidden = !s.station_location;
  const liveNow = s.signals.poll_live;
  if (rec) {
    $("#phase").textContent = liveNow ? "Live now" : "Waiting for broadcast";
    $("#now-panel").className = `panel phase-${liveNow ? "stopped" : "waiting"}`;
  }
  $("#station-detail").textContent = rec
    ? (liveNow ? `On air; the speaker is playing ${rec.kind === "quran" ? "the Quran" : "a recording"}. Play now to listen live.`
      : `Last heard ${lowerFirst(timeAgo(s.last_heard_ms))}. The live broadcast takes over when it starts.`)
    : detailLine(s);
  $("#play").hidden = active;
  $("#stop").hidden = !active;
  miniplayer.update($("#now-player"), s);
  // The Pi mirrors whatever it plays (live or a recording) to the browser, in sync.
  const onAir = s.signals.poll_live || ["playing", "reconnecting", "recording"].includes(s.phase);
  $("#listen-here").hidden = !s.browser_listening || !onAir || local.isPlaying();
  $("#autoplay").checked = s.autoplay;
  if (document.activeElement !== $("#volume")) $("#volume").value = s.volume;
  paintVolume();

  // Only speak up when something needs attention (details live on Speaker and System).
  const alert = alertFor(s);
  $("#now-alert").hidden = !alert;
  if (alert) $("#now-alert").innerHTML = alert;

  const blocked = $("#site-blocked");
  blocked.hidden = !s.site_blocked_until_ms;
  if (s.site_blocked_until_ms) {
    blocked.innerHTML = `<b>livemasjid.com is refusing requests from this network.</b> Playback is unaffected: it uses the
      public stream servers. The instant live signal and directory updates are paused until about
      ${clockTime(s.site_blocked_until_ms)}. If this persists, ask the livemasjid team to lift the block.`;
  }
  document.title = s.phase === "playing" ? `● ${s.station_name ?? "Live Masjid"}` : "Live Masjid";
}

/** One short warning for the home screen, or "" when all is well. */
function alertFor(s) {
  if (s.site_blocked_until_ms) return ""; // has its own notice
  if (s.bt.needs_repair) return 'Your speaker forgot this Pi. Put it in pairing mode and <a href="#speaker">pair it again</a>.';
  if (!s.output.open && s.output.error) return 'No speaker is connected, so nothing can play. <a href="#speaker">Check the speaker</a>.';
  const sig = s.signals;
  const silentFor = Date.now() - Math.max(sig.poll_last_ms || 0, s.started_ms || 0);
  if (s.mount && !sig.poll_ok && !sig.sse_connected && silentFor > 3 * 60 * 1000) {
    return `Can't reach the livemasjid servers to watch for broadcasts. Check the Pi's internet connection on <a href="#system">System</a>.`;
  }
  return "";
}
