// "History": the broadcasts heard, for the followed masjid or all masjids. Each shows the salah it
// belonged to ("Esha"), or an estimate for older ones ("around Esha"). The Pi keeps a year.
//
// Filters: a period (7 days / 30 days / Year) and a salah. In the Year view this month is listed in
// full and earlier months collapse to one summary row each; tapping one shows that month (as a
// removable chip).

import { api } from "../api.js";
import { emptyState, icon } from "../components.js";
import { $, $$, setActive } from "../dom.js";
import { clockTime, dayHeading, duration, escapeHtml, timeAgo } from "../format.js";
import { getStatus } from "../store.js";

const PAGE = 150;
const DAY_MS = 864e5;

const state = {
  scope: "selected", // or "all"
  period: "7", // "7" | "30" | "year"
  salah: "", // "" = all
  range: null, // { from, to, label } from a month row
  limit: PAGE,
  days: {}, // "YYYY-MM-DD" → { count, ms }, the whole year (for the month rows)
};

/** The masjid's short name: "Lakewood Estate Masjid, Ormonde, Jhb, RSA" → "Lakewood Estate Masjid". */
const shortName = (name) => String(name || "").split(",")[0].trim();
const pad = (n) => String(n).padStart(2, "0");
const monthKey = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}`;
function firstOfMonth(d) { return new Date(d.getFullYear(), d.getMonth(), 1); }
const startOfDay = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate());
const monthName = (d) => d.toLocaleDateString([], { month: "long", year: "numeric" });

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

/** Broadcasts grouped under day headings with each day's total. */
function dayGroups(sessions, named) {
  const days = new Map();
  for (const s of sessions) {
    const heading = dayHeading(s.start_ms);
    if (!days.has(heading)) days.set(heading, []);
    days.get(heading).push(s);
  }
  let html = "";
  for (const [heading, list] of days) {
    const total = list.reduce((sum, s) => sum + (s.end_ms - s.start_ms), 0);
    html += `<div class="group-label history-day"><span>${escapeHtml(heading)}</span><span>${list.length} · ${escapeHtml(duration(total))}</span></div>`;
    html += list.map((s) => historyRow(s, named)).join("");
  }
  return html;
}

/** One collapsed row per month before `before` (newest first), from the per-day totals. */
function monthRows(before) {
  const months = new Map();
  for (const [day, t] of Object.entries(state.days)) {
    const m = day.slice(0, 7);
    if (m >= before) continue;
    const cur = months.get(m) ?? { count: 0, ms: 0 };
    months.set(m, { count: cur.count + t.count, ms: cur.ms + t.ms });
  }
  return [...months.entries()].sort((a, b) => b[0].localeCompare(a[0])).map(([m, t]) => {
    const d = new Date(Number(m.slice(0, 4)), Number(m.slice(5, 7)) - 1, 1);
    return `<button class="item clickable h-month" data-month="${m}">
      <span class="grow item-title">${escapeHtml(monthName(d))}</span>
      <span class="text-2 small tabular">${t.count} · ${escapeHtml(duration(t.ms))}</span>${icon("chevron")}
    </button>`;
  }).join("");
}

/** The query for the current filters: [from, to] in ms (to may be null). */
function queryWindow() {
  if (state.range) return [state.range.from, state.range.to];
  const now = Date.now();
  if (state.period === "year") return [firstOfMonth(new Date()).getTime(), null]; // this month in full
  return [startOfDay(new Date(now - (Number(state.period) - 1) * DAY_MS)).getTime(), null];
}

function selectRange(from, to, label) {
  state.range = { from: from.getTime(), to: to.getTime(), label };
  state.limit = PAGE;
  load();
}

function selectMonth(d) {
  const from = firstOfMonth(d);
  selectRange(from, new Date(from.getFullYear(), from.getMonth() + 1, 1), monthName(from));
}

export function init() {
  $("#history-filter").addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    state.scope = btn.dataset.scope;
    state.limit = PAGE;
    setActive($$("#history-filter button"), btn);
    load();
  });
  $("#h-period").addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    state.period = btn.dataset.period;
    state.range = null;
    state.limit = PAGE;
    setActive($$("#h-period button"), btn);
    load();
  });
  $("#h-salah").addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    state.salah = btn.dataset.salah;
    state.limit = PAGE;
    setActive($$("#h-salah button"), btn);
    load();
  });
  $("#h-range-clear").addEventListener("click", () => {
    state.range = null;
    state.limit = PAGE;
    load();
  });
  $("#history").addEventListener("click", (e) => {
    const row = e.target.closest("[data-month]");
    if (!row) return;
    const [y, m] = row.dataset.month.split("-").map(Number);
    selectMonth(new Date(y, m - 1, 1));
  });
  $("#history-more").addEventListener("click", () => {
    state.limit += PAGE;
    load();
  });
}

export function show() {
  load();
}

async function load() {
  const status = getStatus();
  const mount = status?.mount;
  // Name the followed masjid instead of "This masjid".
  $("#history-mine").textContent = shortName(status?.station_name) || "This masjid";
  const [from, to] = queryWindow();
  const q = new URLSearchParams({ limit: state.limit, from });
  if (to) q.set("to", to);
  if (state.scope === "selected" && mount) q.set("mount", mount);
  if (state.salah) q.set("salah", state.salah);
  $("#h-range").hidden = !state.range;
  $("#h-range-label").textContent = state.range?.label ?? "";
  try {
    const r = await api(`history?${q}`);
    state.days = r.days || {};
    $("#history-last").textContent = !r.last ? "Never" : r.last.live ? "Live now" : timeAgo(r.last.ms);
    // The Year view's totals cover the whole year, not just the month listed in full.
    const yearView = state.period === "year" && !state.range;
    const totals = yearView
      ? Object.values(state.days).reduce((a, t) => ({ count: a.count + t.count, ms: a.ms + t.ms }), { count: 0, ms: 0 })
      : { count: r.total, ms: r.listened_ms };
    $("#history-count").textContent = totals.count;
    $("#history-time").textContent = duration(totals.ms);

    const named = !(state.scope === "selected" && mount);
    let html = dayGroups(r.sessions, named);
    if (yearView) {
      const months = monthRows(monthKey(new Date()));
      if (months) html += `<div class="group-label">Earlier this year</div>${months}`;
    }
    const filtered = state.salah || state.range;
    $("#history").innerHTML = html || emptyState(filtered
      ? "No broadcasts match these filters."
      : "No broadcasts yet. They appear here automatically.");
    $("#history-more").hidden = r.sessions.length >= r.total;
  } catch (err) {
    $("#history").innerHTML = emptyState(err.message);
  }
}
