// "Settings": stream options, directory, password, system actions and activity log.

import { api } from "../api.js";
import { $, closeModal, openModal, runAction, toast } from "../dom.js";
import { escapeHtml, lowerFirst, timeAgo } from "../format.js";
import { subscribe } from "../store.js";

export function init() {
  $("#codec").addEventListener("change", (e) => runAction(null, () => api("settings", { codec: e.target.value }), "Saved"));
  $("#poll").addEventListener("change", (e) => runAction(null, () => api("settings", { poll_secs: Number(e.target.value) }), "Saved"));
  $("#refresh-directory").addEventListener("click", (e) =>
    runAction(e.currentTarget, () => api("stations/refresh", {}), (r) => `Directory updated · ${r.count} streams`).then(loadDirectoryInfo));
  $("#set-password").addEventListener("click", () => {
    $("#new-password").value = "";
    openModal("password-modal", "#new-password");
  });
  $("#password-form").addEventListener("submit", savePassword);
  $("#logout").addEventListener("click", async () => {
    await api("logout", {}).catch(() => {});
    location.reload();
  });
  $("#check-updates").addEventListener("click", (e) => runAction(e.currentTarget, () => api("system/update", {}), "Checking for updates"));
  $("#restart").addEventListener("click", (e) => {
    if (confirm("Restart the player service?")) runAction(e.currentTarget, () => api("system/restart", {}), "Restarting");
  });
  $("#refresh-logs").addEventListener("click", loadLogs);
  subscribe(render);
}

export function show() {
  loadConfig();
  loadDirectoryInfo();
  loadLogs();
}

/** "vipIatina VSW07 · Bluetooth (A2DP)" rather than the raw ALSA device name. */
function outputLabel(out) {
  if (!out.open) return out.error ? "Unavailable" : "Idle";
  const kind = out.kind === "bluetooth" ? "Bluetooth (A2DP)" : out.kind === "wired" ? "Wired" : out.kind;
  return `${out.label} · ${kind}`;
}

function render(s) {
  $("#password-info").textContent = s.password_set ? "This page is password protected" : "Anyone on your network can open this page";
  $("#set-password").textContent = s.password_set ? "Change password" : "Set password";
  $("#logout-row").hidden = !s.password_set;
  const rows = [
    ["Version", s.version],
    ["Running since", timeAgo(s.started_ms)],
    ["Output", outputLabel(s.output)],
    ...(s.stream_url ? [["Stream", s.stream_url]] : []),
  ];
  $("#system-info").innerHTML = rows.map(([k, v]) => `<dt>${k}</dt><dd>${escapeHtml(v)}</dd>`).join("");
}

async function loadConfig() {
  try {
    const c = await api("config");
    $("#codec").value = c.codec;
    $("#poll").value = String(c.poll_secs);
  } catch { /* keep previous values */ }
}

async function loadDirectoryInfo() {
  try {
    const { stations, fetched_ms } = await api("stations");
    $("#directory-info").textContent = fetched_ms
      ? `${stations.length} streams · updated ${lowerFirst(timeAgo(fetched_ms))}`
      : `${stations.length} streams · built-in list`;
  } catch { /* ignore */ }
}

async function loadLogs() {
  try {
    const { logs } = await api("logs");
    const time = (t) => new Date(t).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit" });
    $("#logs").innerHTML = logs.map((l) => `<div class="${l.level}"><span class="text-3">${time(l.t)}</span>  ${escapeHtml(l.msg)}</div>`).join("")
      || '<div class="text-3">No activity yet.</div>';
  } catch { /* ignore */ }
}

async function savePassword(e) {
  e.preventDefault();
  const value = $("#new-password").value;
  try {
    await api("password", { new_password: value });
    closeModal("password-modal");
    toast(value ? "Password saved" : "Password removed");
  } catch (err) {
    toast(err.message);
  }
}
