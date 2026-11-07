// Salah times card on the home screen (data: masjidscreen.co.za via the Pi's cache).
// Highlights the next salah with a countdown; falls back to a short note when the masjid
// doesn't publish times or the site can't be reached.

import { api } from "../api.js";
import { $, $$ } from "../dom.js";
import { escapeHtml } from "../format.js";

let data = null;
let loadedFor = null;
let retryTimer;

/** Load (or reload) times for `mount`. The Pi caches, so this is cheap to call. */
export async function load(mount) {
  if (!mount) {
    $("#salah").innerHTML = "";
    $("#salah-title").hidden = true;
    $("#salah").hidden = true;
    return;
  }
  loadedFor = mount;
  try {
    data = await api(`salah?mount=${encodeURIComponent(mount)}`);
  } catch {
    data = { status: "error", message: "Couldn't load salah times" };
  }
  if (loadedFor === mount) render();
}

/** Re-render (countdown) without fetching. */
export function tick() {
  if (data && data.status === "ok") render();
}

function at(hhmm, dayOffset = 0) {
  const [h, m] = hhmm.split(":").map(Number);
  const d = new Date();
  d.setDate(d.getDate() + dayOffset);
  d.setHours(h, m, 0, 0);
  return d;
}

function inWords(ms) {
  const mins = Math.max(0, Math.round(ms / 60000));
  if (mins < 1) return "now";
  if (mins < 60) return `in ${mins} min`;
  return `in ${Math.floor(mins / 60)} h ${String(mins % 60).padStart(2, "0")} min`;
}

/** Today's salah list, with Jumu'ah replacing Zuhr on Fridays. */
function todays(d) {
  const friday = new Date().getDay() === 5;
  return d.times.map((t) => (t.key === "zuhr" && friday && d.jummah ? { ...d.jummah, key: "zuhr" } : t));
}

/** The next athaan or jamaat after now: { index, kind, when } (or Fajr tomorrow). */
function nextEvent(list) {
  const now = Date.now();
  for (let i = 0; i < list.length; i++) {
    for (const kind of ["athaan", "jamaat"]) {
      const t = list[i][kind];
      if (t && at(t).getTime() > now) return { index: i, kind, when: at(t) };
    }
  }
  const fajr = list[0];
  const t = fajr && (fajr.athaan || fajr.jamaat);
  return t ? { index: 0, kind: fajr.athaan ? "athaan" : "jamaat", when: at(t, 1), tomorrow: true } : null;
}

function render() {
  const box = $("#salah");
  $("#salah-title").hidden = false;
  box.hidden = false;
  const source = $("#salah-source");
  source.href = data.source || "https://masjidscreen.co.za/";

  if (data.status !== "ok") {
    // Fallback: the card stays, with a calm one-line explanation (and a retry for errors).
    const retry = data.status === "error" || data.status === "blocked"
      ? ' <button class="btn ghost small" id="salah-retry">Retry</button>' : "";
    box.innerHTML = `<div class="empty small">${escapeHtml(data.message || "Salah times unavailable")}${retry}</div>`;
    $("#salah-retry")?.addEventListener("click", () => load(loadedFor));
    clearTimeout(retryTimer);
    if (data.status === "error") retryTimer = setTimeout(() => load(loadedFor), 10 * 60 * 1000);
    return;
  }

  const list = todays(data);
  const next = nextEvent(list);
  const now = Date.now();
  const cells = list.map((s, i) => {
    const last = s.jamaat || s.athaan;
    const past = !next?.tomorrow && last && at(last).getTime() < now;
    const cls = next && next.index === i && !next.tomorrow ? "next" : past ? "past" : "";
    const jamaat = s.jamaat && s.jamaat !== s.athaan ? `Jamaat ${s.jamaat}` : "&nbsp;";
    return `<div class="salah-cell ${cls}">
      <div class="name">${escapeHtml(s.label)}</div>
      <div class="athaan">${escapeHtml(s.athaan || s.jamaat || "—")}</div>
      <div class="jamaat">${jamaat}</div>
    </div>`;
  }).join("");
  let line = "";
  if (next) {
    const s = list[next.index];
    const what = next.kind === "jamaat" ? `${s.label} jamaat` : `${s.label} athaan`;
    line = next.tomorrow
      ? `Next: <b>${escapeHtml(s.label)}</b> tomorrow at ${escapeHtml(s.athaan || s.jamaat)}`
      : `Next: <b>${escapeHtml(what)}</b> ${inWords(next.when.getTime() - now)} (${next.when.toTimeString().slice(0, 5)})`;
  }
  box.innerHTML = `<div class="salah-grid">${cells}</div><div class="salah-next">${line}</div>`;
}

export function hide() {
  for (const el of $$("#salah, #salah-title")) el.hidden = true;
}
