// "Masjids": searchable directory with live badges; choosing one starts following it.

import { api } from "../api.js";
import { $, $$, setActive, toast } from "../dom.js";
import { escapeHtml, lowerFirst, timeAgo } from "../format.js";

const GENRES = ["Masjid", "Institution", "Aalim"];
let stations = [];
let filter = "all";
let searchTimer;
let navigate = () => {};

export function init(go) {
  navigate = go;
  $("#search").addEventListener("input", () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(render, 80);
  });
  $("#filters").addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    filter = btn.dataset.filter;
    setActive($$("#filters button"), btn);
    render();
  });
  $("#stations").addEventListener("click", (e) => {
    const rec = e.target.closest("[data-recordings]");
    if (rec) { location.hash = `recordings/${rec.dataset.recordings}`; return; }
    const row = e.target.closest(".item[data-mount]");
    if (row) choose(row.dataset.mount);
  });
  // "/" jumps to search, like most directory UIs.
  document.addEventListener("keydown", (e) => {
    const onMasjids = $("#view-masjids").classList.contains("active");
    if (e.key === "/" && onMasjids && !e.target.closest("input, select, textarea")) {
      e.preventDefault();
      $("#search").focus();
    }
  });
}

export async function show() {
  try {
    ({ stations } = await api("stations"));
    render();
  } catch (err) {
    $("#stations").innerHTML = `<div class="empty">${escapeHtml(err.message)}</div>`;
  }
}

function matches(s, words) {
  if (filter === "live" && !s.live) return false;
  if (filter === "heard" && !s.last_heard_ms) return false;
  if (GENRES.includes(filter) && s.genre !== filter) return false;
  const haystack = `${s.name} ${s.location} ${s.mount}`.toLowerCase();
  return words.every((w) => haystack.includes(w));
}

function compare(a, b) {
  return (b.selected - a.selected)
    || (filter === "heard" ? (b.last_heard_ms ?? 0) - (a.last_heard_ms ?? 0) : 0)
    || (b.live - a.live)
    || a.name.localeCompare(b.name);
}

function row(s) {
  const heard = s.last_heard_ms ? ` · heard ${escapeHtml(lowerFirst(timeAgo(s.last_heard_ms)))}` : "";
  return `<div class="item clickable" data-mount="${escapeHtml(s.mount)}">
    <div class="grow">
      <div class="item-title truncate">${escapeHtml(s.name)}</div>
      <div class="item-meta truncate">${s.selected ? "Selected · " : ""}${escapeHtml(s.location || s.genre)}${heard}</div>
    </div>
    ${s.live ? '<span class="live-tag">LIVE</span>' : ""}
    ${s.selected ? '<svg class="check"><use href="#i-check"/></svg>' : ""}
    <button class="btn ghost small" data-recordings="${escapeHtml(s.mount)}" title="Recordings">Recordings</button>
  </div>`;
}

function render() {
  const words = $("#search").value.trim().toLowerCase().split(/\s+/).filter(Boolean);
  const list = stations.filter((s) => matches(s, words)).sort(compare);
  const liveCount = stations.filter((s) => s.live).length;
  $("#result-count").textContent = `${list.length} of ${stations.length} · ${liveCount} live now`;
  $("#stations").innerHTML = list.length ? list.map(row).join("") : '<div class="empty">No masjids match your search.</div>';
}

async function choose(mount) {
  const station = stations.find((s) => s.mount === mount);
  if (!station) return;
  if (station.selected) return navigate("now");
  try {
    await api("select", { mount });
    for (const s of stations) s.selected = s.mount === mount;
    toast(`Following ${station.name}${station.live ? " (live now)" : ""}`);
    navigate("now");
  } catch (err) {
    toast(err.message);
  }
}
