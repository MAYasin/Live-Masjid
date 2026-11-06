// "History": every broadcast heard, grouped by day, with 7-day totals.

import { api } from "../api.js";
import { $, $$, setActive } from "../dom.js";
import { clockTime, dayHeading, duration, escapeHtml, shortDate, timeAgo } from "../format.js";
import { getStatus } from "../store.js";

let scope = "selected";

/** One broadcast as a list row (also used on the Now playing page). */
export function historyRow(s) {
  const live = !s.reason;
  const reconnects = s.reconnects ? ` · ${s.reconnects} reconnect${s.reconnects > 1 ? "s" : ""}` : "";
  return `<div class="item">
    <div class="grow">
      <div class="item-title truncate">${escapeHtml(s.name)}</div>
      <div class="item-meta">${clockTime(s.start_ms)} – ${live ? "now" : clockTime(s.end_ms)} · ${duration(s.end_ms - s.start_ms)}${reconnects}</div>
    </div>
    ${live ? '<span class="live-tag">LIVE</span>' : `<span class="xsmall text-3">${escapeHtml(shortDate(s.start_ms))}</span>`}
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

    let html = "";
    let day = "";
    for (const s of sessions) {
      const heading = dayHeading(s.start_ms);
      if (heading !== day) {
        day = heading;
        html += `<div class="group-label">${escapeHtml(heading)}</div>`;
      }
      html += historyRow(s);
    }
    $("#history").innerHTML = html || '<div class="empty">No broadcasts recorded yet.</div>';
  } catch (err) {
    $("#history").innerHTML = `<div class="empty">${escapeHtml(err.message)}</div>`;
  }
}
