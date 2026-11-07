// "Settings": stream options, masjid directory and password.

import { api } from "../api.js";
import { $, closeModal, openModal, runAction, toast } from "../dom.js";
import { lowerFirst, timeAgo } from "../format.js";
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
  subscribe(render);
}

export function show() {
  loadConfig();
  loadDirectoryInfo();
}

function render(s) {
  $("#password-info").textContent = s.password_set ? "This page is password protected" : "Anyone on your network can open this page";
  $("#set-password").textContent = s.password_set ? "Change password" : "Set password";
  $("#logout-row").hidden = !s.password_set;
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
