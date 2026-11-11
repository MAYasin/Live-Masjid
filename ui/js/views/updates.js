// Settings › Updates: the installed version, "Check for updates" (asks GitHub), "Update now"
// (installs, even over a test build) and the automatic-updates switch. While an update runs, the
// page polls its status and reloads once the new version is running.

import { api } from "../api.js";
import { pollWhileOpen } from "../components.js";
import { $, runAction, toast } from "../dom.js";
import { lowerFirst, timeAgo } from "../format.js";

let updates = null; // { installed, auto_update, last, latest }
let updating = null; // { from, at } after "Update now"

export function init() {
  $("#check-updates").addEventListener("click", (e) => checkUpdates(e.currentTarget));
  $("#update-now").addEventListener("click", (e) => runAction(e.currentTarget, () => api("system/update", {}), "Updating…").then(() => {
    updating = { from: updates?.installed, at: Date.now() };
    paint();
  }));
  $("#auto-update").addEventListener("change", (e) =>
    runAction(null, () => api("settings", { auto_update: e.target.checked }), e.target.checked ? "Automatic updates on" : "Automatic updates off"));
}

export function show() {
  load();
  // Follow a running update while this page is open.
  pollWhileOpen("updates", () => updating && load(), 3000);
}

async function load(check = false) {
  try {
    const u = await api(`system/updates${check ? "?check=true" : ""}`);
    // A new version took over: reload so the page runs its matching interface.
    if (updating && u.installed !== updating.from) {
      toast(`Updated to ${u.installed}`);
      setTimeout(() => location.reload(), 1500);
    }
    if (!check && updates?.latest) u.latest = updates.latest; // keep the last check's answer
    updates = u;
    paint();
    return u;
  } catch (err) {
    if (check) throw err;
    return null; // the service may be restarting during an update
  }
}

function checkUpdates(btn) {
  return runAction(btn, async () => {
    const u = await load(true);
    const newer = u.latest && u.latest.tag !== u.installed;
    return newer ? `${u.latest.tag} is available` : "You're up to date";
  }, (msg) => msg);
}

/** The line under "Version": what's happening, or what last happened. */
function describe({ installed, auto_update, last, latest }) {
  const newer = latest && latest.tag !== installed && latest.available;
  if (updating) {
    const state = last && last.at * 1000 >= updating.at - 5000 ? last.state : "checking";
    if (["failed", "up_to_date", "skipped"].includes(state)) updating = null;
    if (state === "failed") return last.message;
    if (state === "up_to_date") return "Already up to date";
    return `Updating${latest?.tag ? ` to ${latest.tag}` : ""}… the page reloads when it's done`;
  }
  if (newer) return `${latest.tag} is available`;
  if (latest) return "Up to date";
  if (last?.at) {
    const when = lowerFirst(timeAgo(last.at * 1000));
    return {
      updated: `Updated to ${last.to} ${when}`,
      up_to_date: `Up to date, checked ${when}`,
      failed: last.message,
      skipped: last.message,
      updating: `Updating to ${last.to}…`,
      checking: "Checking…",
    }[last.state] ?? "";
  }
  return auto_update ? "Checks for a new version every hour" : "Automatic updates are off";
}

function paint() {
  if (!updates) return;
  const { installed, auto_update, latest } = updates;
  $("#upd-version").textContent = installed;
  $("#auto-update").checked = auto_update;
  const newer = latest && latest.tag !== installed && latest.available;
  $("#update-now").hidden = !newer || Boolean(updating);
  $("#update-now").textContent = newer ? `Update to ${latest.tag}` : "Update now";
  $("#upd-info").textContent = describe(updates);
}
