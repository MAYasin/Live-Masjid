// "History": every broadcast heard, grouped by day, with 7-day totals.

import { api } from "../api.js";
import { $, $$, setActive } from "../dom.js";
import { clockTime, dayHeading, duration, escapeHtml, timeAgo } from "../format.js";
import { getStatus } from "../store.js";

let scope = "selected";

/** One broadcast as a list row. With `named`, the masjid's name leads (all masjids). */
export function historyRow(s, named = true) {
  const live = !s.reason;
  const span = `${clockTime(s.start_ms)} – ${live ? "now" : clockTime(s.end_ms)}`;
  const reconnects = s.reconnects ? ` · ${s.reconnects} reconnect${s.reconnects > 1 ? "s" : ""}` : "";
  const meta = `${duration(s.end_ms - s.start_ms)}${reconnects}`;
  return `<div class="item">
    <div class="grow">
      <div class="item-title truncate">${escapeHtml(named ? s.name : span)}</div>
      <div class="item-meta">${named ? `${span} · ${meta}` : meta}</div>
    </div>
    ${live ? '<span class="live-tag">LIVE</span>' : ""}
  </div>`;
}

export function init() {
  $("#history-filter").addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    scope = btn.dataset.scope;
    setActive($$("#history-filter button"), btn);
    show();
  });
}

export async function show() {
  const mount = getStatus()?.mount;
  const filter = scope === "selected" && mount ? `&mount=${encodeURIComponent(mount)}` : "";
  try {
    const { sessions } = await api(`history?limit=300${filter}`);
    const week = sessions.filter((s) => s.start_ms > Date.now() - 7 * 864e5);
    $("#history-last").textContent = sessions.length ? (sessions[0].reason ? timeAgo(sessions[0].end_ms) : "Live now") : "Never";
    $("#history-count").textContent = week.length;
    $("#history-time").textContent = duration(week.reduce((sum, s) => sum + (s.end_ms - s.start_ms), 0));

    // Group by day; each day shows its total listening time.
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
    $("#history").innerHTML = html || '<div class="empty">No broadcasts recorded yet.</div>';
  } catch (err) {
    $("#history").innerHTML = `<div class="empty">${escapeHtml(err.message)}</div>`;
  }
}
