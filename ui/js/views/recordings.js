// Recordings: the selected masjid's latest on the home screen, and a full list per masjid
// (#recordings/<mount>). Tapping one plays it on the Pi speaker.

import { api } from "../api.js";
import { emptyState } from "../components.js";
import { $, runAction, toast } from "../dom.js";
import { duration, escapeHtml, fileSize, shortDate } from "../format.js";
import { getStatus } from "../store.js";
import * as local from "../local.js";
import * as miniplayer from "../miniplayer.js";

let currentMount = null;

function meta(r) {
  const parts = [];
  // livemasjid titles already contain the date ("Recording Sat, 07 Nov …"); don't repeat it.
  if (r.published && !r.title.includes(r.published)) {
    const d = new Date(r.published);
    parts.push(isNaN(d) ? r.published : `${shortDate(d.getTime())}, ${d.toTimeString().slice(0, 5)}`);
  }
  if (r.duration_s) parts.push(duration(r.duration_s * 1000));
  if (r.size_bytes) parts.push(fileSize(r.size_bytes));
  return parts.join(" · ");
}

/** List rows with a Play button each. */
export function rows(list, limit) {
  const items = limit ? list.items.slice(0, limit) : list.items;
  const here = Boolean(getStatus()?.browser_listening);
  return items.map((r) => `
    <div class="item wrap">
      <div class="grow">
        <div class="item-title truncate">${escapeHtml(r.title)}</div>
        <div class="item-meta truncate">${escapeHtml(meta(r))}</div>
      </div>
      <div class="item-actions">
        ${here ? `<button class="btn small ghost" data-play-here="${escapeHtml(r.id)}" data-mount="${escapeHtml(list.mount)}">Play here</button>` : ""}
        <button class="btn small" data-play-recording="${escapeHtml(r.id)}" data-mount="${escapeHtml(list.mount)}">Play on speaker</button>
      </div>
    </div>`).join("");
}

/** Fallback text for a non-ok list. */
export function fallback(list) {
  const msg = list?.message || "Recordings are unavailable right now.";
  return emptyState(msg, { small: true });
}

/** Handle a click on any "Play on speaker" button (event delegation). */
export function onPlayClick(e) {
  const here = e.target.closest("[data-play-here]");
  if (here) {
    runAction(here, () => local.playRecording(here.dataset.mount, here.dataset.playHere));
    return;
  }
  const btn = e.target.closest("[data-play-recording]");
  if (!btn) return;
  // Show it in the player card at once; the Pi confirms (or the card goes back) moments later.
  const title = btn.closest(".item")?.querySelector(".item-title")?.textContent ?? "Recording";
  miniplayer.starting(title, "recording");
  api("recordings/play", { mount: btn.dataset.mount, id: btn.dataset.playRecording })
    .catch((err) => { miniplayer.cancelStarting(); toast(err.message); });
}

export function init() {
  $("#rec-list").addEventListener("click", onPlayClick);
  $("#rec-refresh").addEventListener("click", () => show(currentMount));
}

export async function show(mount) {
  currentMount = mount || getStatus()?.mount || null;
  const heading = $("#rec-heading");
  if (!currentMount) {
    heading.textContent = "Recordings";
    $("#rec-list").innerHTML = emptyState("Choose a masjid first.");
    return;
  }
  heading.textContent = `Recordings · ${currentMount}`;
  $("#rec-list").innerHTML = '<div class="empty"><span class="spinner"></span></div>';
  $("#rec-notice").hidden = true;
  try {
    const [list, stations] = await Promise.all([
      api(`recordings?mount=${encodeURIComponent(currentMount)}`),
      api("stations").catch(() => null),
    ]);
    const name = stations?.stations.find((s) => s.mount === currentMount)?.name;
    if (name) heading.textContent = `Recordings · ${name}`;
    if (list.status === "ok") {
      $("#rec-list").innerHTML = rows(list);
    } else {
      $("#rec-list").innerHTML = fallback(list);
    }
  } catch (err) {
    $("#rec-list").innerHTML = fallback({ message: err.message });
  }
}
