// "History": the broadcasts heard, for the followed masjid or all masjids, grouped by day with
// each day's listening time. Each broadcast shows the salah it belonged to ("Esha"), or an
// estimate for older ones ("around Esha"). The Pi keeps a year of history.

import { api } from "../api.js";
import { emptyState } from "../components.js";
import { $, $$, setActive } from "../dom.js";
import { clockTime, dayHeading, duration, escapeHtml, timeAgo } from "../format.js";
import { getStatus } from "../store.js";

const PAGE = 150;
let scope = "selected";
let limit = PAGE;

/** The masjid's short name: "Lakewood Estate Masjid, Ormonde, Jhb, RSA" → "Lakewood Estate Masjid". */
const shortName = (name) => String(name || "").split(",")[0].trim();

function salahTag(s) {
  if (!s.salah) return "";
  return s.salah_estimated
    ? `<span class="salah-tag estimated" title="Estimated from the masjid's current times">around ${escapeHtml(s.salah)}</span>`
    : `<span class="salah-tag">${escapeHtml(s.salah)}</span>`;
}

/** One broadcast. With `named`, the masjid's name is shown too (all masjids). */
export function historyRow(s, named = true) {
  const live = !s.reason;
  const span = `${clockTime(s.start_ms)} – ${live ? "now" : clockTime(s.end_ms)}`;
  const reconnects = s.reconnects ? `${s.reconnects} reconnect${s.reconnects > 1 ? "s" : ""}` : "";
  const meta = [named ? shortName(s.name) : "", reconnects].filter(Boolean).join(" · ");
  return `<div class="item h-row">
    <div class="grow">
      <div class="item-title">${salahTag(s)}<span class="tabular">${span}</span></div>
      ${meta ? `<div class="item-meta truncate">${escapeHtml(meta)}</div>` : ""}
    </div>
    ${live ? '<span class="live-tag">LIVE</span>' : `<span class="h-dur tabular">${duration(s.end_ms - s.start_ms)}</span>`}
  </div>`;
}

export function init() {
  $("#history-filter").addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    scope = btn.dataset.scope;
    limit = PAGE;
    setActive($$("#history-filter button"), btn);
    show();
  });
  $("#history-more").addEventListener("click", () => {
    limit += PAGE;
    show();
  });
}

export async function show() {
  const status = getStatus();
  const mount = status?.mount;
  // Name the followed masjid instead of "This masjid".
  $("#history-mine").textContent = shortName(status?.station_name) || "This masjid";
  const filter = scope === "selected" && mount ? `&mount=${encodeURIComponent(mount)}` : "";
  try {
    const { sessions } = await api(`history?limit=${limit}${filter}`);
    const week = sessions.filter((s) => s.start_ms > Date.now() - 7 * 864e5);
    $("#history-last").textContent = sessions.length ? (sessions[0].reason ? timeAgo(sessions[0].end_ms) : "Live now") : "Never";
    $("#history-count").textContent = week.length;
    $("#history-time").textContent = duration(week.reduce((sum, s) => sum + (s.end_ms - s.start_ms), 0));

    const named = !(scope === "selected" && mount);
    const days = new Map();
    for (const s of sessions) {
      const heading = dayHeading(s.start_ms);
      if (!days.has(heading)) days.set(heading, []);
      days.get(heading).push(s);
    }
    let html = "";
    for (const [heading, list] of days) {
      const total = list.reduce((sum, s) => sum + (s.end_ms - s.start_ms), 0);
      html += `<div class="group-label history-day"><span>${escapeHtml(heading)}</span><span>${escapeHtml(duration(total))}</span></div>`;
      html += list.map((s) => historyRow(s, named)).join("");
    }
    $("#history").innerHTML = html || emptyState("No broadcasts yet. They appear here automatically.");
    // Fewer than asked for means there is nothing older (the Pi keeps a year).
    $("#history-more").hidden = sessions.length < limit;
  } catch (err) {
    $("#history").innerHTML = emptyState(err.message);
  }
}
