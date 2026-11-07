// "System": live health of the device — Wi-Fi, Bluetooth, CPU/memory/temperature/disk —
// plus the app's own usage, update/restart actions and the activity log.

import { api } from "../api.js";
import { $, runAction } from "../dom.js";
import { escapeHtml } from "../format.js";

const REFRESH_MS = 3000;
let timer;

export function init() {
  $("#check-updates").addEventListener("click", (e) => runAction(e.currentTarget, () => api("system/update", {}), "Checking for updates"));
  $("#restart").addEventListener("click", (e) => {
    if (confirm("Restart the player service?")) runAction(e.currentTarget, () => api("system/restart", {}), "Restarting");
  });
  $("#refresh-logs").addEventListener("click", loadLogs);
}

export function show() {
  refresh();
  loadLogs();
  clearInterval(timer);
  // Poll only while the page is visible; stops by itself when another view is opened.
  timer = setInterval(() => {
    if ($("#view-system").classList.contains("active") && !document.hidden) refresh();
    else clearInterval(timer);
  }, REFRESH_MS);
}

/** Uptime as "3 d 4 h", "5 h 12 m" or "8 m". */
function uptime(s) {
  const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  if (d) return `${d} d ${h} h`;
  if (h) return `${h} h ${m} m`;
  return `${m} m`;
}

function meter(id, percent, warnAt = 75, critAt = 90) {
  const bar = $(`#${id}-bar`);
  const p = Math.max(0, Math.min(100, percent ?? 0));
  bar.style.width = `${p}%`;
  bar.className = p >= critAt ? "critical" : p >= warnAt ? "high" : "";
}

/** Render [label, value] rows, skipping empty values. */
function kv(id, rows) {
  $(`#${id}`).innerHTML = rows
    .filter(([, v]) => v !== null && v !== undefined && v !== "")
    .map(([k, v]) => `<dt>${escapeHtml(k)}</dt><dd>${escapeHtml(v)}</dd>`)
    .join("");
}

const yesNo = (b) => (b ? "Yes" : "No");
const fixed = (n, d = 1) => (n == null ? null : Number(n).toFixed(d));

async function refresh() {
  let s;
  try { s = await api("system"); } catch { return; }
  const { device: dev, wifi, bluetooth: bt, app } = s;

  const memPct = dev.mem_total_mb ? (100 * dev.mem_used_mb) / dev.mem_total_mb : 0;
  $("#sys-cpu").textContent = dev.cpu_percent == null ? "…" : `${fixed(dev.cpu_percent, 0)}%`;
  meter("sys-cpu", dev.cpu_percent);
  $("#sys-mem").textContent = `${dev.mem_used_mb} / ${dev.mem_total_mb} MB`;
  meter("sys-mem", memPct);
  $("#sys-temp").textContent = dev.temp_c == null ? "—" : `${fixed(dev.temp_c, 0)} °C`;
  meter("sys-temp", dev.temp_c == null ? 0 : (dev.temp_c / 85) * 100, 70, 85); // Pi throttles at 80–85 °C
  $("#sys-signal").textContent = wifi?.signal_dbm == null ? "—" : `${wifi.signal_dbm} dBm`;
  const signal = $("#sys-signal-bar");
  signal.style.width = `${wifi?.quality ?? 0}%`;
  signal.className = (wifi?.quality ?? 100) < 40 ? "critical" : (wifi?.quality ?? 100) < 60 ? "high" : "";

  const power = [];
  if (dev.undervoltage_now) power.push("Under-voltage right now: the power supply is too weak, which causes Wi-Fi/Bluetooth drop-outs and crashes. Use a proper 5 V supply.");
  else if (dev.undervoltage_since_boot) power.push("Under-voltage happened since boot: the power supply may be too weak.");
  if (dev.throttled_now) power.push("The CPU is being throttled (heat or power).");
  $("#sys-power").hidden = power.length === 0;
  $("#sys-power").textContent = power.join(" ");

  kv("sys-network", wifi ? [
    ["Wi-Fi network", wifi.ssid ?? "Not connected"],
    ["Signal", wifi.signal_dbm == null ? null : `${wifi.signal_dbm} dBm (${wifi.quality}%) ${signalWord(wifi.signal_dbm)}`],
    ["Band", wifi.frequency_mhz ? `${(wifi.frequency_mhz / 1000).toFixed(1)} GHz (${wifi.frequency_mhz} MHz)` : null],
    ["Link speed", wifi.tx_bitrate],
    ["IP address", wifi.ip],
    ["Power saving", wifi.power_save == null ? null : wifi.power_save ? "On (may cause drop-outs)" : "Off"],
    ["Interface", wifi.interface],
  ] : [["Wi-Fi", "No wireless interface"]]);

  kv("sys-bluetooth", [
    ["Controller", bt.controller ? `${bt.controller} · ${bt.powered ? "on" : "off"}` : "None"],
    ["Speaker", bt.speaker ?? "None chosen"],
    ["Connected", bt.speaker ? yesNo(bt.connected) : null],
    ["Battery", bt.battery == null ? null : `${bt.battery}%`],
    ["Audio link", bt.transport],
    ["Quality", { medium: "Low", high: "High" }[bt.quality] ?? bt.quality],
  ]);

  kv("sys-device", [
    ["Board", dev.model],
    ["Operating system", dev.os],
    ["Kernel", dev.kernel],
    ["Hostname", dev.hostname],
    ["CPU cores", dev.cpu_cores],
    ["Load average", dev.load.map((l) => l.toFixed(2)).join("  ")],
    ["Storage", `${fixed(dev.disk_used_gb)} / ${fixed(dev.disk_total_gb)} GB used`],
    ["Power", dev.undervoltage_now == null ? null : dev.undervoltage_now ? "Under-voltage!" : "OK"],
    ["Up for", uptime(dev.uptime_s)],
  ]);

  kv("sys-app", [
    ["Version", app.version],
    ["Running for", uptime(app.uptime_s)],
    ["Memory", `${fixed(app.memory_mb)} MB`],
    ["CPU", app.cpu_percent == null ? "…" : `${fixed(app.cpu_percent)}% of one core`],
    ["Threads", app.threads],
    ["Service restarts", app.restarts],
    ["Audio downloaded", `${fixed(app.data_received_mb)} MB since start`],
    ["Buffer", app.phase === "playing" || app.phase === "reconnecting" ? `${fixed(app.buffer_s)} s` : null],
  ]);
}

function signalWord(dbm) {
  if (dbm >= -55) return "· excellent";
  if (dbm >= -65) return "· good";
  if (dbm >= -72) return "· fair";
  return "· weak";
}

async function loadLogs() {
  try {
    const { logs } = await api("logs");
    const time = (t) => new Date(t).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit" });
    $("#logs").innerHTML = logs.map((l) => `<div class="${l.level}"><span class="text-3">${time(l.t)}</span>  ${escapeHtml(l.msg)}</div>`).join("")
      || '<div class="text-3">No activity yet.</div>';
  } catch { /* ignore */ }
}
