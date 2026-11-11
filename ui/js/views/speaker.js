// "Speaker": Bluetooth pairing and audio output settings.

import { api } from "../api.js";
import { emptyState, icon } from "../components.js";
import { $, runAction, toast } from "../dom.js";
import { escapeHtml } from "../format.js";
import { getStatus, subscribe } from "../store.js";

const SCAN_SECONDS = 20;
let scanTimer;

export function init() {
  $("#repair").addEventListener("click", (e) => {
    const mac = getStatus()?.bt.device;
    if (mac) runAction(e.currentTarget, () => api("bt/repair", { mac }), (r) => r.message || "Re-paired").then(loadDevices);
  });
  $("#test-sound").addEventListener("click", (e) => runAction(e.currentTarget, () => api("player/test", {}), "Playing test sound"));
  $("#scan").addEventListener("click", startScan);
  $("#devices").addEventListener("click", onDeviceAction);
  $("#fallback").addEventListener("change", (e) => runAction(null, () => api("settings", { fallback_output: e.target.value }), "Saved"));
  $("#bt-quality").addEventListener("change", (e) =>
    runAction(null, () => api("settings", { bt_quality: e.target.value }), "Saved. The speaker reconnects in a few seconds."));
  $("#keep-awake").addEventListener("change", (e) => runAction(null, () => api("settings", { keep_awake: e.target.checked }), "Saved"));
  subscribe(render);
}

export function show() {
  loadDevices();
  loadOutputs();
}

function speakerState(bt, output) {
  if (bt.recovering) return "Resetting the Bluetooth chip…";
  if (!bt.device) return "Choose a speaker below";
  if (!bt.powered) return "Bluetooth is off · recovering automatically";
  if (bt.needs_repair) return "Needs re-pairing";
  if (!bt.connected) return "Not connected · will connect when the speaker is on";
  const battery = bt.battery != null ? ` · Battery ${bt.battery}%` : "";
  return `Connected${battery}${output.kind === "bluetooth" ? " · Playing here" : ""}`;
}

function render(s) {
  const { bt, output } = s;
  $("#speaker-name").textContent = bt.name || bt.device || "No Bluetooth speaker";
  $("#speaker-state").textContent = speakerState(bt, output);
  $("#speaker-repair").hidden = !bt.needs_repair;
  const error = bt.error || (!bt.needs_repair && !output.open && output.error) || "";
  $("#speaker-error").hidden = !error;
  $("#speaker-error").textContent = error;
  $("#keep-awake").checked = s.keep_awake;
  const scan = $("#scan");
  scan.disabled = bt.scanning || !bt.powered;
  scan.innerHTML = bt.scanning ? '<span class="spinner"></span>Scanning' : `${icon("refresh")}Scan`;
}

function deviceRow(d) {
  const state = d.connected ? "Connected" : d.paired ? "Paired" : "Available";
  const primary = d.preferred
    ? `<button class="btn small" data-action="stop" data-mac="${d.mac}">Stop using</button>`
    : `<button class="btn small primary" data-action="use" data-mac="${d.mac}">${d.paired ? "Use" : "Pair"}</button>`;
  const forget = d.paired ? `<button class="btn small ghost" data-action="forget" data-mac="${d.mac}">Forget</button>` : "";
  return `<div class="item wrap">
    <div class="grow">
      <div class="item-title truncate">${escapeHtml(d.name || "Unnamed device")}</div>
      <div class="item-meta truncate">${d.preferred ? "Your speaker · " : ""}${state} · ${escapeHtml(d.mac)}</div>
    </div>
    <div class="item-actions">${primary}${forget}</div>
  </div>`;
}

async function loadDevices() {
  try {
    const { devices } = await api("bt/devices");
    const named = devices.filter((d) => d.name || d.paired);
    const hidden = devices.length - named.length;
    const rows = named.length
      ? named.map(deviceRow).join("")
      : emptyState("No speakers found yet. Put your speaker in pairing mode and tap Scan.");
    const note = hidden ? `<div class="item"><div class="item-meta">${hidden} unnamed nearby device${hidden > 1 ? "s" : ""} hidden</div></div>` : "";
    $("#devices").innerHTML = rows + note;
  } catch (err) {
    $("#devices").innerHTML = emptyState(err.message);
  }
}

async function startScan() {
  try {
    await api("bt/scan", {});
    toast(`Scanning for ${SCAN_SECONDS} seconds`);
    clearInterval(scanTimer);
    let ticks = 0;
    scanTimer = setInterval(() => {
      loadDevices();
      if (++ticks > SCAN_SECONDS / 2) clearInterval(scanTimer);
    }, 2000);
  } catch (err) {
    toast(err.message);
  }
}

function onDeviceAction(e) {
  const btn = e.target.closest("button[data-action]");
  if (!btn) return;
  const { action, mac } = btn.dataset;
  if (action === "forget" && !confirm("Forget this device? You'll need to pair it again.")) return;
  const messages = { use: (r) => r.message || "Connected", stop: "Speaker released", forget: "Device forgotten" };
  runAction(btn, () => api(`bt/${action}`, { mac }), messages[action]).then(loadDevices);
}

async function loadOutputs() {
  try {
    const [{ cards }, config] = await Promise.all([api("outputs"), api("config")]);
    const options = [["auto", "Automatic"], ["none", "None"], ...cards.map((c) => [c.device, c.label])];
    if (!options.some(([v]) => v === config.fallback_output)) options.push([config.fallback_output, config.fallback_output]);
    const select = $("#fallback");
    select.innerHTML = options.map(([v, label]) => `<option value="${escapeHtml(v)}">${escapeHtml(label)}</option>`).join("");
    select.value = config.fallback_output;
    $("#bt-quality").value = config.bt_quality ?? "high";
  } catch { /* leave previous options */ }
}
