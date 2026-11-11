// Settings › Playback: how far behind live, listening in the browser, and advanced stream options.

import { api } from "../api.js";
import { $, runAction } from "../dom.js";
import { lowerFirst, timeAgo } from "../format.js";

const save = (patch, message = "Saved") => runAction(null, () => api("settings", patch), message);

export function init() {
  $("#live-delay").addEventListener("change", (e) => save({ live_delay_s: Number(e.target.value) }));
  $("#browser-listening").addEventListener("change", (e) =>
    save({ browser_listening: e.target.checked }, e.target.checked ? "Visitors can now listen on their devices" : "Listening in the browser turned off"));
  $("#codec").addEventListener("change", (e) => save({ codec: e.target.value }));
  $("#poll").addEventListener("change", (e) => save({ poll_secs: Number(e.target.value) }));
  $("#refresh-directory").addEventListener("click", (e) =>
    runAction(e.currentTarget, () => api("stations/refresh", {}), (r) => `Directory updated · ${r.count} streams`).then(loadDirectoryInfo));
}

export function show() {
  loadConfig();
  loadDirectoryInfo();
}

async function loadConfig() {
  try {
    const c = await api("config");
    $("#codec").value = c.codec;
    $("#poll").value = String(c.poll_secs);
    $("#browser-listening").checked = Boolean(c.browser_listening);
    const delay = String(c.live_delay_s ?? 3);
    // A custom value (set via the API) still shows up.
    if (![...$("#live-delay").options].some((o) => o.value === delay)) $("#live-delay").add(new Option(`${delay} s`, delay));
    $("#live-delay").value = delay;
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
