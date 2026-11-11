// "Settings": a hub of short rows (Speaker, Playback, Device & app, Updates, Password), each with
// its current state in one line, opening a focused page.

import { api } from "../api.js";
import { $ } from "../dom.js";
import { subscribe } from "../store.js";

const DELAYS = { 3: "Near live", 8: "Balanced", 20: "Extra steady" };

export function init() {
  subscribe(renderFromStatus);
}

export async function show() {
  // The rest comes from the live status; these two are fetched when the hub opens.
  try {
    const [config, sys] = await Promise.all([api("config"), api("system")]);
    const delay = DELAYS[config.live_delay_s] ?? `${config.live_delay_s} s behind live`;
    $("#hub-playback").textContent = `${delay}${config.browser_listening ? ", browser listening on" : ""}`;
    const app = sys.app;
    $("#hub-system").textContent = `App: ${Number(app.cpu_percent ?? 0).toFixed(1)}% CPU, ${Number(app.memory_mb).toFixed(1)} MB` +
      (sys.device.temp_c != null ? ` · Pi ${Math.round(sys.device.temp_c)} °C` : "");
  } catch { /* rows keep their last text */ }
  try {
    const u = await api("system/updates");
    $("#hub-updates").textContent = `${u.installed}${u.auto_update ? ", automatic" : ", automatic updates off"}`;
  } catch { /* ignore */ }
}

function renderFromStatus(s) {
  const out = s.output;
  const battery = out.kind === "bluetooth" && s.bt.battery != null ? `, battery ${s.bt.battery}%` : "";
  $("#hub-speaker").textContent = out.open ? `${out.label}${battery}` : s.bt.needs_repair ? "Needs pairing again" : "Not connected";
  $("#hub-security").textContent = s.password_set ? "Set" : "Not set: anyone on your network can open this page";
}
